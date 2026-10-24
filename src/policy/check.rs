//! The checks a policy must pass before it's committed.
//!
//! Every check here runs on the leader (and in `relish lint` and
//! `relish policy test`), never in the state machine: whether a policy
//! compiles depends on the `cel` crate's version, and the state machine
//! must give the same answer on every node for ever.

use std::collections::BTreeSet;

use cel::common::ast::{EntryExpr, Expr, IdedExpr};

use crate::config::PolicySpec;

/// The longest `match` or `deny` expression a policy may hold, in bytes.
///
/// CEL has no loops of its own, so evaluation is bounded by the size of the
/// expression times the size of the input. The `cel` crate tracks no cost,
/// so this cap is the bound on the expression half (chapter 18).
pub const MAX_EXPRESSION_BYTES: usize = 1024;

/// The longest `message` a policy may hold, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 512;

/// The largest `default` table a policy may hold, in bytes of JSON.
pub const MAX_DEFAULT_BYTES: usize = 4096;

/// App fields a `default` can never fill, and why.
pub const UNFILLABLE_FIELDS: [(&str, &str); 4] = [
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
];

/// The variables a policy expression can see. Nothing else: no other apps,
/// no policies, permissions or namespaces, no clock.
pub const POLICY_VARIABLES: [&str; 3] = ["app", "caller", "source"];

/// The fields of `app`: every `[app.*]` key, plus the app's `name`.
///
/// `namespace` is always present, resolved to `default` when the spec
/// doesn't name one. A test checks this list against `AppSpec` itself.
pub const APP_FIELDS: [&str; 34] = [
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
    /// `policy`, `permission` or `namespace`.
    #[error(
        "{field} uses {variable:?}, but policies see only app, caller and source; they never match policies, permissions or namespaces"
    )]
    UnknownVariable {
        /// The expression.
        field: PolicyField,
        /// The variable it names.
        variable: String,
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
        compile_checked(PolicyField::Match, expression)?;
    }
    if let Some(deny) = &spec.deny {
        compile_checked(PolicyField::Deny, deny)?;
    }
    match &spec.default {
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
    check_names(field, program.expression(), &mut Vec::new())?;
    Ok(program)
}

/// Walk the expression, refusing a free variable a policy can't see and a
/// field `app` or `caller` doesn't have. `bound` holds the variables the
/// enclosing comprehensions (`all`, `exists`, `map`, `filter`) introduce.
fn check_names(
    field: PolicyField,
    expression: &IdedExpr,
    bound: &mut Vec<String>,
) -> Result<(), PolicyError> {
    match &expression.expr {
        Expr::Unspecified | Expr::Literal(_) => Ok(()),
        Expr::Ident(name) => check_variable(field, name, bound),
        Expr::Select(select) => {
            if let Expr::Ident(variable) = &select.operand.expr
                && !bound.contains(variable)
            {
                check_field(field, variable, &select.field)?;
            }
            check_names(field, &select.operand, bound)
        }
        Expr::Call(call) => {
            if let Some(target) = &call.target {
                check_names(field, target, bound)?;
            }
            call.args
                .iter()
                .try_for_each(|argument| check_names(field, argument, bound))
        }
        Expr::List(list) => list
            .elements
            .iter()
            .try_for_each(|element| check_names(field, element, bound)),
        Expr::Map(map) => map
            .entries
            .iter()
            .try_for_each(|entry| check_entry(field, &entry.expr, bound)),
        Expr::Struct(structure) => structure
            .entries
            .iter()
            .try_for_each(|entry| check_entry(field, &entry.expr, bound)),
        Expr::Comprehension(comprehension) => {
            // The range and the accumulator's start are outside the loop;
            // everything else sees the loop's own variables.
            check_names(field, &comprehension.iter_range, bound)?;
            check_names(field, &comprehension.accu_init, bound)?;
            let depth = bound.len();
            bound.push(comprehension.iter_var.clone());
            bound.extend(comprehension.iter_var2.iter().cloned());
            bound.push(comprehension.accu_var.clone());
            let inner = [
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ]
            .into_iter()
            .try_for_each(|part| check_names(field, part, bound));
            bound.truncate(depth);
            inner
        }
    }
}

fn check_entry(
    field: PolicyField,
    entry: &EntryExpr,
    bound: &mut Vec<String>,
) -> Result<(), PolicyError> {
    match entry {
        EntryExpr::StructField(structure) => check_names(field, &structure.value, bound),
        EntryExpr::MapEntry(map) => {
            check_names(field, &map.key, bound)?;
            check_names(field, &map.value, bound)
        }
    }
}

fn check_variable(field: PolicyField, name: &str, bound: &[String]) -> Result<(), PolicyError> {
    if bound.iter().any(|variable| variable == name)
        || POLICY_VARIABLES.contains(&name)
        || TYPE_NAMES.contains(&name)
    {
        return Ok(());
    }
    Err(PolicyError::UnknownVariable {
        field,
        variable: name.to_string(),
    })
}

fn check_field(field: PolicyField, variable: &str, name: &str) -> Result<(), PolicyError> {
    let known: &[&str] = match variable {
        "app" => &APP_FIELDS,
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
