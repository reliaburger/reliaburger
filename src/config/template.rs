//! Templates that expand to ordinary apps and jobs (#1124).
//!
//! A `[template.NAME]` table declares typed parameters and a body of
//! ordinary `[app.*]` and `[job.*]` tables, with `${param}` placeholders.
//! A `[use.NAME]` table picks a template and gives its parameters values.
//!
//! ```toml
//! [template.postgres]
//! params = { name = "string", size = "string", replicas = { type = "int", default = 1 } }
//! body = """
//! [app.${name}]
//! image = "postgres:17"
//! replicas = ${replicas}
//! [[app.${name}.volumes]]
//! path = "/var/lib/postgresql/data"
//! size = "${size}"
//! """
//!
//! [use.orders-db]
//! template = "postgres"
//! name = "orders-db"
//! size = "50Gi"
//! ```
//!
//! Expansion is substitution and nothing else: no conditionals, loops or
//! functions. It runs in the config compiler that `relish` and Lettuce
//! share, and again on the apply endpoint for a manifest that arrives
//! unexpanded, always before policy. What reaches Raft is ordinary apps and
//! jobs, each with a [`Provenance`] naming the template and the `use` that
//! made it.
//!
//! Substitution is textual but knows where it is in the TOML. Inside a
//! quoted string a value is escaped; outside one, a string value must be a
//! plain key. So a value can never change the shape of the body, the way a
//! bound parameter can't change the shape of an SQL statement.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::Config;

/// What opens a placeholder.
const OPEN: &str = "${";

/// What writes a literal `${` in a body: `$${` stays `${`.
const ESCAPED_OPEN: &str = "$${";

/// The `[use.*]` key that names the template, so no parameter can take it.
const TEMPLATE_KEY: &str = "template";

/// A `[template.NAME]` table: typed parameters and a body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateSpec {
    /// The parameters a `use` gives values to, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamSpec>,
    /// Ordinary `[app.*]` and `[job.*]` TOML with `${param}` placeholders.
    pub body: String,
}

/// The type of a template parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    /// A TOML string.
    String,
    /// A TOML integer.
    Int,
    /// `true` or `false`.
    Bool,
}

impl ParamType {
    /// The name TOML uses for the type: `string`, `int` or `bool`.
    pub fn as_str(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Int => "int",
            ParamType::Bool => "bool",
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "string" => Ok(ParamType::String),
            "int" => Ok(ParamType::Int),
            "bool" => Ok(ParamType::Bool),
            other => Err(format!(
                "unknown parameter type {other:?}: use \"string\", \"int\" or \"bool\""
            )),
        }
    }
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One parameter: its type, and a default when it's optional. A parameter
/// without a default is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawParam", into = "RawParam")]
pub struct ParamSpec {
    /// The type every value must have.
    pub kind: ParamType,
    /// The value a `use` gets when it leaves the parameter out.
    pub default: Option<ParamValue>,
}

/// How a parameter is written: `name = "string"`, or `replicas = { type =
/// "int", default = 1 }`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RawParam {
    Short(String),
    Full(FullParam),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FullParam {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<ParamValue>,
}

impl TryFrom<RawParam> for ParamSpec {
    type Error = String;

    fn try_from(raw: RawParam) -> Result<Self, Self::Error> {
        let (kind, default) = match raw {
            RawParam::Short(kind) => (ParamType::parse(&kind)?, None),
            RawParam::Full(full) => (ParamType::parse(&full.kind)?, full.default),
        };
        if let Some(value) = &default
            && value.kind() != kind
        {
            return Err(format!(
                "the default {value} is {} {}, not {} {kind}",
                article(value.kind()),
                value.kind(),
                article(kind)
            ));
        }
        Ok(ParamSpec { kind, default })
    }
}

impl From<ParamSpec> for RawParam {
    fn from(spec: ParamSpec) -> Self {
        match spec.default {
            None => RawParam::Short(spec.kind.as_str().to_string()),
            Some(default) => RawParam::Full(FullParam {
                kind: spec.kind.as_str().to_string(),
                default: Some(default),
            }),
        }
    }
}

/// A parameter's value, as a `use` gives it or a default holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, expecting = "a string, an integer or a boolean")]
pub enum ParamValue {
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A string.
    String(String),
}

impl ParamValue {
    /// The type this value has.
    pub fn kind(&self) -> ParamType {
        match self {
            ParamValue::Bool(_) => ParamType::Bool,
            ParamValue::Int(_) => ParamType::Int,
            ParamValue::String(_) => ParamType::String,
        }
    }
}

impl fmt::Display for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamValue::Bool(value) => write!(f, "{value}"),
            ParamValue::Int(value) => write!(f, "{value}"),
            ParamValue::String(value) => write!(f, "{value:?}"),
        }
    }
}

fn article(kind: ParamType) -> &'static str {
    match kind {
        ParamType::Int => "an",
        ParamType::String | ParamType::Bool => "a",
    }
}

/// A `[use.NAME]` table: the template to expand and its parameter values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<String, ParamValue>",
    into = "BTreeMap<String, ParamValue>"
)]
pub struct UseSpec {
    /// The template's name.
    pub template: String,
    /// Every other key: a parameter's value.
    pub values: BTreeMap<String, ParamValue>,
}

impl TryFrom<BTreeMap<String, ParamValue>> for UseSpec {
    type Error = String;

    fn try_from(mut values: BTreeMap<String, ParamValue>) -> Result<Self, Self::Error> {
        match values.remove(TEMPLATE_KEY) {
            Some(ParamValue::String(template)) => Ok(UseSpec { template, values }),
            Some(other) => Err(format!(
                "`template` names the template to use, so it's a string, not {other}"
            )),
            None => Err("a [use.*] table needs `template = \"NAME\"`".to_string()),
        }
    }
}

impl From<UseSpec> for BTreeMap<String, ParamValue> {
    fn from(spec: UseSpec) -> Self {
        let mut values = spec.values;
        values.insert(TEMPLATE_KEY.to_string(), ParamValue::String(spec.template));
        values
    }
}

/// Where an app or job came from, when a template made it: the template's
/// name and the `[use.*]` that expanded it. Nothing else is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// The template's name.
    pub template: String,
    /// The `[use.*]` table's name.
    #[serde(rename = "use")]
    pub instance: String,
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "use.{} (template {})", self.instance, self.template)
    }
}

/// Why a template or a `use` was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The template itself is malformed.
    #[error("template {template:?}: {reason}")]
    InvalidTemplate { template: String, reason: String },
    /// The body names a placeholder no parameter fills.
    #[error(
        "template {template:?}: ${{{placeholder}}} isn't one of its parameters, so it would be left unresolved"
    )]
    UnresolvedPlaceholder {
        template: String,
        placeholder: String,
    },
    /// A `use` with a bad name.
    #[error("use.{instance}: {reason}")]
    InvalidUse { instance: String, reason: String },
    /// A `use` of a template nobody declared.
    #[error("use.{instance}: there's no template {template:?}")]
    UnknownTemplate { instance: String, template: String },
    /// A `use` gives a value to a parameter the template doesn't have.
    #[error("use.{instance}: template {template:?} has no parameter {parameter:?}")]
    UnknownParameter {
        instance: String,
        template: String,
        parameter: String,
    },
    /// A `use` leaves out a parameter that has no default.
    #[error("use.{instance}: template {template:?} needs a value for {parameter:?}")]
    MissingParameter {
        instance: String,
        template: String,
        parameter: String,
    },
    /// A value of the wrong type.
    #[error(
        "use.{instance}: parameter {parameter:?} of template {template:?} is {} {expected}, not {} {found}",
        article(*expected),
        article(*found)
    )]
    TypeMismatch {
        instance: String,
        template: String,
        parameter: String,
        expected: ParamType,
        found: ParamType,
    },
    /// A value that can't go where its placeholder is.
    #[error("use.{instance}: parameter {parameter:?} of template {template:?}: {reason}")]
    Placement {
        instance: String,
        template: String,
        parameter: String,
        reason: String,
    },
    /// The expanded body isn't valid config.
    #[error("use.{instance}: template {template:?} doesn't expand to valid config: {reason}")]
    Body {
        instance: String,
        template: String,
        reason: String,
    },
    /// The body declares something other than apps and jobs.
    #[error(
        "use.{instance}: template {template:?} declares [{table}], but a template body holds only [app.*] and [job.*] tables"
    )]
    BodyTable {
        instance: String,
        template: String,
        table: String,
    },
    /// The body declares or uses a template.
    #[error(
        "use.{instance}: template {template:?} declares [{table}]; templates can't declare or use other templates"
    )]
    Nested {
        instance: String,
        template: String,
        table: String,
    },
    /// An expanded name someone else already has.
    #[error("use.{instance} expands to {resource}, which {other} already declares")]
    Clash {
        instance: String,
        resource: String,
        other: String,
    },
}

impl Config {
    /// Expand this config's `[use.*]` tables with its own templates, then
    /// drop both, leaving only ordinary apps and jobs. A config with no
    /// `use` and no template is unchanged.
    pub fn expand_templates(&mut self) -> Result<(), TemplateError> {
        let templates = std::mem::take(&mut self.template);
        for (name, template) in &templates {
            check_template(name, template)?;
        }
        expand(self, &templates)
    }
}

/// Expand `config`'s `[use.*]` tables with `templates`, which the caller
/// has already passed through [`check_template`], into ordinary apps and
/// jobs, each carrying its [`Provenance`]. `config.uses` is empty
/// afterwards; `config.template` is left alone, because the compiler
/// collects templates from every file.
///
/// Uses expand in name order, so the same input always gives the same
/// output, and the first problem found is the error.
pub fn expand(
    config: &mut Config,
    templates: &BTreeMap<String, TemplateSpec>,
) -> Result<(), TemplateError> {
    let uses = std::mem::take(&mut config.uses);
    // Which `use` made each resource so far, to name it in a clash.
    let mut made: BTreeMap<String, String> = BTreeMap::new();
    for (instance, spec) in &uses {
        let body = expand_one(instance, spec, templates)?;
        let provenance = Provenance {
            template: spec.template.clone(),
            instance: instance.clone(),
        };
        for (name, mut app) in body.app {
            let resource = format!("app.{name}");
            claim(&made, config.app.contains_key(&name), &resource, instance)?;
            app.provenance = Some(provenance.clone());
            made.insert(resource, instance.clone());
            config.app.insert(name, app);
        }
        for (name, mut job) in body.job {
            let resource = format!("job.{name}");
            claim(&made, config.job.contains_key(&name), &resource, instance)?;
            job.provenance = Some(provenance.clone());
            made.insert(resource, instance.clone());
            config.job.insert(name, job);
        }
    }
    Ok(())
}

/// Refuse `resource` for `instance` when another `use` made it, or the
/// config declares it itself.
fn claim(
    made: &BTreeMap<String, String>,
    declared: bool,
    resource: &str,
    instance: &str,
) -> Result<(), TemplateError> {
    let other = match made.get(resource) {
        Some(earlier) => format!("use.{earlier}"),
        None if declared => format!("a declared {}", resource_kind(resource)),
        None => return Ok(()),
    };
    Err(TemplateError::Clash {
        instance: instance.to_string(),
        resource: resource.to_string(),
        other,
    })
}

fn resource_kind(resource: &str) -> &str {
    resource.split('.').next().unwrap_or(resource)
}

/// Expand one `use` into the config its body declares.
fn expand_one(
    instance: &str,
    spec: &UseSpec,
    templates: &BTreeMap<String, TemplateSpec>,
) -> Result<Config, TemplateError> {
    if !super::valid_workload_label(instance) {
        return Err(TemplateError::InvalidUse {
            instance: instance.to_string(),
            reason: format!("{instance:?} must be a lowercase DNS label"),
        });
    }
    let template = templates
        .get(&spec.template)
        .ok_or_else(|| TemplateError::UnknownTemplate {
            instance: instance.to_string(),
            template: spec.template.clone(),
        })?;
    let values = bind(instance, spec, template)?;
    let text = substitute(&template.body, &values).map_err(|(parameter, reason)| {
        TemplateError::Placement {
            instance: instance.to_string(),
            template: spec.template.clone(),
            parameter: parameter.to_string(),
            reason,
        }
    })?;
    let body = Config::parse(&text).map_err(|error| TemplateError::Body {
        instance: instance.to_string(),
        template: spec.template.clone(),
        reason: error.to_string(),
    })?;
    check_body(instance, &spec.template, &body)?;
    Ok(body)
}

/// Give every parameter its value: the `use`'s, or the default.
fn bind<'a>(
    instance: &str,
    spec: &'a UseSpec,
    template: &'a TemplateSpec,
) -> Result<BTreeMap<&'a str, &'a ParamValue>, TemplateError> {
    if let Some(parameter) = spec
        .values
        .keys()
        .find(|key| !template.params.contains_key(*key))
    {
        return Err(TemplateError::UnknownParameter {
            instance: instance.to_string(),
            template: spec.template.clone(),
            parameter: parameter.clone(),
        });
    }
    let mut values = BTreeMap::new();
    for (parameter, param) in &template.params {
        let value = spec
            .values
            .get(parameter)
            .or(param.default.as_ref())
            .ok_or_else(|| TemplateError::MissingParameter {
                instance: instance.to_string(),
                template: spec.template.clone(),
                parameter: parameter.clone(),
            })?;
        if value.kind() != param.kind {
            return Err(TemplateError::TypeMismatch {
                instance: instance.to_string(),
                template: spec.template.clone(),
                parameter: parameter.clone(),
                expected: param.kind,
                found: value.kind(),
            });
        }
        values.insert(parameter.as_str(), value);
    }
    Ok(values)
}

/// A body declares apps and jobs, and nothing else.
fn check_body(instance: &str, template: &str, body: &Config) -> Result<(), TemplateError> {
    let nested = body
        .template
        .keys()
        .map(|name| format!("template.{name}"))
        .chain(body.uses.keys().map(|name| format!("use.{name}")))
        .next();
    if let Some(table) = nested {
        return Err(TemplateError::Nested {
            instance: instance.to_string(),
            template: template.to_string(),
            table,
        });
    }
    let other = body
        .namespace
        .keys()
        .map(|name| format!("namespace.{name}"))
        .chain(
            body.permission
                .keys()
                .map(|name| format!("permission.{name}")),
        )
        .chain(body.policy.keys().map(|name| format!("policy.{name}")))
        .chain(body.build.keys().map(|name| format!("build.{name}")))
        .next();
    match other {
        Some(table) => Err(TemplateError::BodyTable {
            instance: instance.to_string(),
            template: template.to_string(),
            table,
        }),
        None => Ok(()),
    }
}

/// Check a template on its own: its name, its parameters' names, and that
/// every placeholder in its body names a parameter.
pub fn check_template(name: &str, template: &TemplateSpec) -> Result<(), TemplateError> {
    let invalid = |reason: String| TemplateError::InvalidTemplate {
        template: name.to_string(),
        reason,
    };
    if !super::valid_workload_label(name) {
        return Err(invalid(format!(
            "{name:?} must be a lowercase DNS label of 1–63 bytes, with letters or digits at both ends and only letters, digits or hyphens within"
        )));
    }
    for parameter in template.params.keys() {
        if parameter == TEMPLATE_KEY {
            return Err(invalid(
                "a parameter can't be called `template`, because that key names the template in a [use.*] table".to_string(),
            ));
        }
        if !valid_parameter_name(parameter) {
            return Err(invalid(format!(
                "parameter names start with a letter or '_' and hold only letters, digits, '_' and '-'; {parameter:?} doesn't"
            )));
        }
    }
    for piece in scan(&template.body).map_err(invalid)? {
        if let Piece::Placeholder {
            name: placeholder, ..
        } = piece
            && !template.params.contains_key(placeholder)
        {
            return Err(TemplateError::UnresolvedPlaceholder {
                template: name.to_string(),
                placeholder: placeholder.to_string(),
            });
        }
    }
    Ok(())
}

fn valid_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// ---------------------------------------------------------------------------
// Substitution
// ---------------------------------------------------------------------------

/// Where in the TOML the scanner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    /// Outside any string or comment: keys, numbers, booleans.
    Bare,
    /// `"..."`
    Basic,
    /// `"""..."""`
    MultiBasic,
    /// `'...'`
    Literal,
    /// `'''...'''`
    MultiLiteral,
    /// From `#` to the end of the line.
    Comment,
}

/// The kinds of place a placeholder can stand in. A comment isn't one:
/// placeholders in comments are left as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Bare,
    /// A basic string, one line or many: a value is escaped.
    Basic,
    Literal,
    MultiLiteral,
}

/// A piece of a body: text to copy, or a placeholder to fill.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    Placeholder { name: &'a str, slot: Slot },
}

/// Split `body` into text and placeholders, tracking which kind of TOML
/// string each placeholder sits in. `$${` is the text `${`.
fn scan(body: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut pieces = Vec::new();
    let mut context = Context::Bare;
    let mut start = 0;
    let mut index = 0;
    while let Some(next) = body[index..].chars().next() {
        let rest = &body[index..];
        if context != Context::Comment {
            if rest.starts_with(ESCAPED_OPEN) {
                pieces.push(Piece::Text(&body[start..index]));
                pieces.push(Piece::Text(OPEN));
                index += ESCAPED_OPEN.len();
                start = index;
                continue;
            }
            if let Some(after) = rest.strip_prefix(OPEN) {
                let name = placeholder_name(after)?;
                pieces.push(Piece::Text(&body[start..index]));
                pieces.push(Piece::Placeholder {
                    name,
                    slot: slot(context),
                });
                index += OPEN.len() + name.len() + 1;
                start = index;
                continue;
            }
        }
        let (after, step) = advance(context, rest);
        context = after;
        index += step.max(next.len_utf8());
    }
    pieces.push(Piece::Text(&body[start..]));
    Ok(pieces)
}

/// The name between `${` and `}`.
fn placeholder_name(after: &str) -> Result<&str, String> {
    let end = after
        .find(['}', '\n'])
        .filter(|end| after[*end..].starts_with('}'))
        .ok_or_else(|| "an unclosed placeholder: `${` needs a `}` on the same line".to_string())?;
    let name = &after[..end];
    if name.is_empty() {
        return Err("an empty placeholder `${}`".to_string());
    }
    if !valid_parameter_name(name) {
        return Err(format!(
            "`${{{name}}}` isn't a placeholder: parameter names start with a letter or '_' and hold only letters, digits, '_' and '-' (write `$${{` for a literal `${{`)"
        ));
    }
    Ok(name)
}

fn slot(context: Context) -> Slot {
    match context {
        Context::Bare | Context::Comment => Slot::Bare,
        Context::Basic | Context::MultiBasic => Slot::Basic,
        Context::Literal => Slot::Literal,
        Context::MultiLiteral => Slot::MultiLiteral,
    }
}

/// The context after the token at the start of `rest`, and how many bytes
/// the token takes (0 means one character).
fn advance(context: Context, rest: &str) -> (Context, usize) {
    match context {
        Context::Bare if rest.starts_with('#') => (Context::Comment, 0),
        Context::Bare if rest.starts_with("\"\"\"") => (Context::MultiBasic, 3),
        Context::Bare if rest.starts_with('"') => (Context::Basic, 0),
        Context::Bare if rest.starts_with("'''") => (Context::MultiLiteral, 3),
        Context::Bare if rest.starts_with('\'') => (Context::Literal, 0),
        // A backslash escapes the next character, which may be a quote.
        Context::Basic | Context::MultiBasic if rest.starts_with('\\') => {
            let escaped = rest[1..].chars().next().map_or(0, char::len_utf8);
            (context, 1 + escaped)
        }
        Context::Basic if rest.starts_with('"') => (Context::Bare, 0),
        Context::MultiBasic if rest.starts_with("\"\"\"") => (Context::Bare, 3),
        Context::Literal if rest.starts_with('\'') => (Context::Bare, 0),
        Context::MultiLiteral if rest.starts_with("'''") => (Context::Bare, 3),
        Context::Comment if rest.starts_with('\n') => (Context::Bare, 0),
        _ => (context, 0),
    }
}

/// Fill every placeholder in `body`. An error names the parameter and why
/// its value can't go where its placeholder is.
fn substitute<'a>(
    body: &str,
    values: &BTreeMap<&'a str, &ParamValue>,
) -> Result<String, (&'a str, String)> {
    // `check_template` ran first, so the body scans and every placeholder
    // names a parameter; `bind` gave every parameter a value.
    let pieces = scan(body).map_err(|reason| ("", reason))?;
    let mut text = String::with_capacity(body.len());
    for piece in pieces {
        match piece {
            Piece::Text(part) => text.push_str(part),
            Piece::Placeholder { name, slot } => {
                let (parameter, value) = values
                    .get_key_value(name)
                    .ok_or(("", format!("${{{name}}} has no value")))?;
                let rendered = render(value, slot).map_err(|reason| (*parameter, reason))?;
                text.push_str(&rendered);
            }
        }
    }
    Ok(text)
}

/// A value as TOML text for its slot.
fn render(value: &ParamValue, slot: Slot) -> Result<String, String> {
    let text = match value {
        ParamValue::Int(number) => return Ok(number.to_string()),
        ParamValue::Bool(flag) => return Ok(flag.to_string()),
        ParamValue::String(text) => text,
    };
    match slot {
        Slot::Basic => Ok(escape_basic(text)),
        Slot::Bare => {
            if !text.is_empty()
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                Ok(text.clone())
            } else {
                Err(format!(
                    "its placeholder is outside quotes, so the value must be a plain key of letters, digits, '-' and '_', and {text:?} isn't; put the placeholder in quotes"
                ))
            }
        }
        Slot::Literal | Slot::MultiLiteral => {
            let multi = slot == Slot::MultiLiteral;
            let fits =
                |c: char| c != '\'' && (!c.is_control() || c == '\t' || (multi && c == '\n'));
            if text.chars().all(fits) {
                Ok(text.clone())
            } else {
                Err(format!(
                    "{text:?} has a quote or a control character, which a literal string ('...') can't hold; use a basic string (\"...\")"
                ))
            }
        }
    }
}

/// Escape `text` for a TOML basic string, so no value can close the string.
fn escape_basic(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests;
