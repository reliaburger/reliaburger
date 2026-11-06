//! The tool list, derived from `relish agent`'s own definition.
//!
//! Nothing here names a tool. [`derive`] walks the clap command tree of
//! [`AgentOptions`](crate::relish::agent::AgentOptions): every leaf command
//! that has a `data` schema becomes a tool, its flags and positionals become
//! the input schema, and its `relish agent schema` becomes the output schema.
//! A command that lands in `relish agent` later is a tool the moment it
//! merges, with no change here, unless [`EXCLUDED`] names it.
//!
//! The one decision a new command can't make for itself is whether it's safe
//! to hand to an agent unattended, so that's the list here. `relish mcp`
//! reads private cluster state and workload-written text, so none of its
//! tools may change anything or send anything anywhere but the cluster API:
//! a tool that could would turn a prompt injection in a log line into an
//! action. The tools/list snapshot in the tests is where a reviewer sees a
//! new tool arrive.

use std::any::TypeId;
use std::collections::BTreeSet;
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

use crate::relish::agent::{self, CommandName};

/// A `relish agent` command that isn't a tool, and why.
#[derive(Debug, Clone, Copy)]
pub struct Exclusion {
    /// The command as a caller types it after `relish agent`.
    pub command: &'static str,
    /// Why it isn't safe to hand to an agent through MCP.
    pub why: &'static str,
}

/// The commands `relish mcp` never offers. Some don't exist yet; naming them
/// now means they can't slip in as tools when they land.
pub const EXCLUDED: &[Exclusion] = &[
    Exclusion {
        command: "source extract",
        why: "writes the source tree into a local directory",
    },
    Exclusion {
        command: "report",
        why: "writes a report bundle into local files, for a human to read and post",
    },
    Exclusion {
        command: "patch",
        why: "builds binaries and starts a throwaway cluster",
    },
    Exclusion {
        command: "upgrade-check",
        why: "fetches release metadata from the internet, beyond the cluster API, \
              and --metadata reads any local file",
    },
    Exclusion {
        command: "setup",
        why: "writes the configuration of the user's agents",
    },
];

/// The exclusion for `command`, if there is one. An excluded group, such as
/// `patch`, excludes every command under it (`patch init`, `patch try`).
pub fn exclusion(command: &str) -> Option<&'static Exclusion> {
    EXCLUDED.iter().find(|excluded| {
        command == excluded.command
            || command
                .strip_prefix(excluded.command)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// The shape of one argument's value, as JSON and on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// A flag that takes no value: `true` passes it.
    Switch,
    /// A whole number.
    Integer,
    /// One string, or one of a fixed list of strings.
    Text,
    /// Several strings.
    TextList,
}

/// One argument of a tool: a JSON property and where it goes on the
/// command line.
#[derive(Debug, Clone)]
pub struct ArgumentSpec {
    /// The property name, clap's id for the argument (`max_bytes`).
    pub property: String,
    /// `--long` for a flag, `None` for a positional.
    pub long: Option<String>,
    /// Where a positional sits, from clap.
    pub index: Option<usize>,
    /// What the value looks like.
    pub kind: ValueKind,
}

/// A tool, with what it takes to run it as a `relish agent` command line.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// The command as a caller types it after `relish agent`: `source grep`.
    pub command: String,
    /// The group `--toolsets` selects by: the first word of the command.
    pub toolset: String,
    /// Whether the answer can carry text a workload wrote, in an `untrusted`
    /// object. `--no-logs` drops these tools.
    pub returns_workload_text: bool,
    /// The arguments, in clap's order.
    pub arguments: Vec<ArgumentSpec>,
    /// Arguments `relish mcp` fixed, such as `--namespace`'s; the caller
    /// can't pass them and the input schema doesn't list them.
    pub pinned: JsonObject,
    /// The tool as `tools/list` describes it.
    pub tool: Tool,
}

/// The tool name for a command: its words joined with `_`, as MCP names
/// allow only letters, digits, `_`, `-` and `.`.
pub fn tool_name(command: &str) -> String {
    command.replace([' ', '-'], "_")
}

/// The `relish agent` command tree, as clap builds it.
pub fn agent_command() -> clap::Command {
    // `Args` is the trait `#[derive(clap::Args)]` implemented for
    // `AgentOptions`; `augment_args` adds its flags and subcommands to a
    // command we name ourselves.
    <agent::AgentOptions as clap::Args>::augment_args(clap::Command::new("agent"))
}

/// The `data` schema of the `relish agent` command `command`, or `None` for
/// a command that doesn't answer with data.
pub fn data_schema(command: &str) -> Option<Value> {
    let name = command_name(command)?;
    agent::data_schema(name)
}

/// The envelope's name for `command`.
pub fn command_name(command: &str) -> Option<CommandName> {
    <CommandName as clap::ValueEnum>::from_str(&command.replace(' ', "-"), false).ok()
}

/// Every tool `tree` offers: each leaf command with a `data` schema that
/// [`EXCLUDED`] doesn't name.
///
/// `tree` is `relish agent`'s clap command ([`agent_command`]) and `schema`
/// gives a command's `data` schema ([`data_schema`]); the tests pass
/// stand-ins for both.
pub fn derive(tree: clap::Command, schema: &dyn Fn(&str) -> Option<Value>) -> Vec<ToolSpec> {
    let mut tree = tree;
    // Building copies global flags (`--max-bytes`, `source --offline`) into
    // every subcommand, so each leaf lists all the flags it accepts.
    tree.build();
    let mut leaves = Vec::new();
    collect_leaves(&tree, &mut Vec::new(), &mut leaves);
    leaves
        .into_iter()
        .filter(|(words, _)| exclusion(&words.join(" ")).is_none())
        .filter_map(|(words, command)| {
            let name = words.join(" ");
            let data = schema(&name)?;
            Some(tool_spec(&words, command, data))
        })
        .collect()
}

/// Every leaf under `command`, with the words that reach it.
fn collect_leaves<'a>(
    command: &'a clap::Command,
    words: &mut Vec<String>,
    leaves: &mut Vec<(Vec<String>, &'a clap::Command)>,
) {
    for sub in command.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        words.push(sub.get_name().to_string());
        if sub.has_subcommands() {
            collect_leaves(sub, words, leaves);
        } else {
            leaves.push((words.clone(), sub));
        }
        words.pop();
    }
}

fn tool_spec(words: &[String], command: &clap::Command, data: Value) -> ToolSpec {
    let name = words.join(" ");
    let arguments: Vec<ArgumentSpec> = command
        .get_arguments()
        .filter(|argument| !is_builtin(argument))
        .map(argument_spec)
        .collect();
    let returns_workload_text = carries_untrusted(&data);
    let description = command
        .get_about()
        .map(|about| about.to_string())
        .unwrap_or_else(|| format!("relish agent {name}"));
    let mut input = input_schema(command);
    with_budget_default(&mut input, &name);
    let mut tool = Tool::new(tool_name(&name), description, Arc::new(input));
    tool.title = Some(format!("relish agent {name}"));
    tool.output_schema = Some(Arc::new(output_schema(data)));
    tool.annotations = Some(
        ToolAnnotations::with_title(format!("relish agent {name}"))
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            // The tool's world is this binary and the cluster it points at.
            .open_world(false),
    );
    ToolSpec {
        command: name,
        toolset: words[0].clone(),
        returns_workload_text,
        arguments,
        pinned: JsonObject::new(),
        tool,
    }
}

/// `--help` and `--version`, which clap adds and a tool never wants.
fn is_builtin(argument: &clap::Arg) -> bool {
    matches!(
        argument.get_action(),
        clap::ArgAction::Help
            | clap::ArgAction::HelpShort
            | clap::ArgAction::HelpLong
            | clap::ArgAction::Version
    ) || argument.is_hide_set()
}

fn argument_spec(argument: &clap::Arg) -> ArgumentSpec {
    ArgumentSpec {
        property: argument.get_id().to_string(),
        long: argument.get_long().map(str::to_string),
        index: argument.get_index(),
        kind: value_kind(argument),
    }
}

fn value_kind(argument: &clap::Arg) -> ValueKind {
    if matches!(argument.get_action(), clap::ArgAction::SetTrue) {
        return ValueKind::Switch;
    }
    let many = matches!(argument.get_action(), clap::ArgAction::Append)
        || argument
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1);
    if many {
        return ValueKind::TextList;
    }
    // A value parser knows the Rust type it produces; clap exposes it as a
    // `TypeId`, Rust's runtime identity for a type.
    let produced = argument.get_value_parser().type_id();
    let integers = [
        TypeId::of::<u8>(),
        TypeId::of::<u16>(),
        TypeId::of::<u32>(),
        TypeId::of::<u64>(),
        TypeId::of::<usize>(),
        TypeId::of::<i32>(),
        TypeId::of::<i64>(),
    ];
    if integers.iter().any(|integer| produced == *integer) {
        ValueKind::Integer
    } else {
        ValueKind::Text
    }
}

/// The JSON Schema of a leaf command's arguments.
fn input_schema(command: &clap::Command) -> JsonObject {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for argument in command.get_arguments().filter(|a| !is_builtin(a)) {
        let id = argument.get_id().to_string();
        if argument.is_required_set() {
            required.push(Value::String(id.clone()));
        }
        properties.insert(id, property_schema(argument));
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    schema
}

/// `--max-bytes` has no clap default because the default depends on the
/// command, so the schema gets this command's from
/// [`agent::default_max_bytes`].
fn with_budget_default(schema: &mut JsonObject, command: &str) {
    let Some(name) = command_name(command) else {
        return;
    };
    if let Some(Value::Object(properties)) = schema.get_mut("properties")
        && let Some(Value::Object(property)) = properties.get_mut("max_bytes")
    {
        property.insert("default".into(), json!(agent::default_max_bytes(name)));
    }
}

fn property_schema(argument: &clap::Arg) -> Value {
    let mut property = Map::new();
    let kind = value_kind(argument);
    match kind {
        ValueKind::Switch => {
            property.insert("type".into(), json!("boolean"));
        }
        ValueKind::Integer => {
            property.insert("type".into(), json!("integer"));
        }
        ValueKind::Text => {
            property.insert("type".into(), json!("string"));
            let choices: Vec<Value> = argument
                .get_possible_values()
                .iter()
                .filter(|choice| !choice.is_hide_set())
                .map(|choice| json!(choice.get_name()))
                .collect();
            if !choices.is_empty() {
                property.insert("enum".into(), Value::Array(choices));
            }
        }
        ValueKind::TextList => {
            property.insert("type".into(), json!("array"));
            property.insert("items".into(), json!({"type": "string"}));
        }
    }
    if let Some(help) = argument.get_long_help().or(argument.get_help()) {
        property.insert("description".into(), json!(help.to_string()));
    }
    if let [default] = argument.get_default_values() {
        let default = default.to_string_lossy();
        let value = match kind {
            ValueKind::Integer => default.parse::<i64>().map(Value::from).ok(),
            ValueKind::Text => Some(Value::String(default.into_owned())),
            ValueKind::Switch | ValueKind::TextList => None,
        };
        if let Some(value) = value {
            property.insert("default".into(), value);
        }
    }
    Value::Object(property)
}

/// The output schema: the envelope's top level, with the command's `data`
/// schema in it. The envelope is what `structuredContent` carries.
///
/// The common parts (`relish`, `error`, `next`) are only sketched, to keep
/// `tools/list` small; `relish agent schema` has them in full.
fn output_schema(data: Value) -> JsonObject {
    let Value::Object(mut data) = data else {
        return Map::new();
    };
    let definitions = data.remove("$defs");
    data.remove("$schema");
    let mut schema = Map::new();
    if let Some(definitions) = definitions {
        schema.insert("$defs".into(), definitions);
    }
    schema.insert("type".into(), json!("object"));
    schema.insert(
        "description".into(),
        json!(
            "The relish.agent/v1 envelope. `ok` says whether the command answered: \
             then `data` is the answer, otherwise `error` says why not."
        ),
    );
    schema.insert(
        "properties".into(),
        json!({
            "schema": {"const": "relish.agent/v1"},
            "command": {"type": "string"},
            "ok": {"type": "boolean"},
            "relish": {"type": "object", "description": "The build of relish that answered."},
            "data": Value::Object(data),
            "truncated": {"type": "boolean", "description": "The answer was trimmed to fit max_bytes; a warning says what was cut."},
            "next": {"type": "array", "description": "Commands worth running next, each with run, why and read_only."},
            "warnings": {"type": "array", "items": {"type": "string"}},
            "error": {"type": "object", "description": "Why the command couldn't answer: code, message, endpoint, status, hint."}
        }),
    );
    schema.insert(
        "required".into(),
        json!(["schema", "command", "ok", "relish"]),
    );
    schema
}

/// Whether a `data` schema can hold an [`Untrusted`] object anywhere.
///
/// [`Untrusted`]: crate::relish::agent::untrusted::Untrusted
pub fn carries_untrusted(data: &Value) -> bool {
    data.pointer("/$defs/Untrusted").is_some()
}

/// Which tools a `relish mcp` offers.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Only these toolsets; all of them when `None`.
    pub toolsets: Option<BTreeSet<String>>,
    /// Leave out every tool whose answer can carry workload-written text.
    pub no_logs: bool,
    /// Fix every `namespace` argument to this one.
    pub namespace: Option<String>,
}

/// Why a [`Selection`] can't be served.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    /// `--toolsets` named a group no tool belongs to.
    #[error("no toolset is called {name:?}; the toolsets are {known}")]
    UnknownToolset {
        /// The name given.
        name: String,
        /// Every toolset, comma-separated.
        known: String,
    },
}

/// Every toolset in `tools`, in order.
pub fn toolsets(tools: &[ToolSpec]) -> BTreeSet<String> {
    tools.iter().map(|tool| tool.toolset.clone()).collect()
}

/// The tools `selection` keeps.
pub fn select(
    tools: Vec<ToolSpec>,
    selection: &Selection,
) -> Result<Vec<ToolSpec>, SelectionError> {
    let known = toolsets(&tools);
    if let Some(wanted) = &selection.toolsets
        && let Some(unknown) = wanted.iter().find(|name| !known.contains(*name))
    {
        return Err(SelectionError::UnknownToolset {
            name: unknown.clone(),
            known: known.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    Ok(tools
        .into_iter()
        .filter(|tool| {
            selection
                .toolsets
                .as_ref()
                .is_none_or(|wanted| wanted.contains(&tool.toolset))
        })
        .filter(|tool| !(selection.no_logs && tool.returns_workload_text))
        .map(|tool| match &selection.namespace {
            Some(namespace) => tool.pin(NAMESPACE, namespace),
            None => tool,
        })
        .collect())
}

/// The argument `--namespace` pins.
const NAMESPACE: &str = "namespace";

impl ToolSpec {
    /// The same tool with `property` fixed to `value`, when it takes it.
    fn pin(mut self, property: &str, value: &str) -> Self {
        if !self.arguments.iter().any(|spec| spec.property == property) {
            return self;
        }
        self.pinned.insert(property.to_string(), json!(value));
        let mut schema = (*self.tool.input_schema).clone();
        if let Some(Value::Object(properties)) = schema.get_mut("properties") {
            properties.remove(property);
        }
        if let Some(Value::Array(required)) = schema.get_mut("required") {
            required.retain(|name| name != property);
        }
        self.tool.input_schema = Arc::new(schema);
        self
    }

    /// The `relish agent` command line (without `relish`) that `arguments`
    /// ask for, or why they don't make one.
    ///
    /// Flags go in as `--flag=value`, so a value starting with `-` can't be
    /// read as another flag, and positionals go after `--` for the same
    /// reason. clap then parses the line exactly as it parses a typed one.
    pub fn command_line(&self, arguments: &JsonObject) -> Result<Vec<String>, String> {
        for key in arguments.keys() {
            if let Some(value) = self.pinned.get(key) {
                return Err(format!(
                    "{key} is fixed to {value} by relish mcp --{key}; leave it out"
                ));
            }
            if !self.arguments.iter().any(|spec| &spec.property == key) {
                let known: Vec<&str> = self
                    .arguments
                    .iter()
                    .map(|spec| spec.property.as_str())
                    .collect();
                return Err(format!(
                    "{} takes no argument {key:?}; it takes: {}",
                    self.tool.name,
                    known.join(", ")
                ));
            }
        }
        let mut line: Vec<String> = std::iter::once("agent".to_string())
            .chain(self.command.split(' ').map(str::to_string))
            .collect();
        let mut positionals: Vec<(usize, Vec<String>)> = Vec::new();
        for spec in &self.arguments {
            let given = self
                .pinned
                .get(&spec.property)
                .or(arguments.get(&spec.property));
            let Some(value) = given.filter(|v| !v.is_null()) else {
                continue;
            };
            let values = spec.values(value)?;
            match (&spec.long, spec.index) {
                (Some(long), _) if spec.kind == ValueKind::Switch => {
                    if value == &Value::Bool(true) {
                        line.push(format!("--{long}"));
                    }
                }
                (Some(long), _) => {
                    line.extend(values.into_iter().map(|value| format!("--{long}={value}")));
                }
                (None, index) => positionals.push((index.unwrap_or(usize::MAX), values)),
            }
        }
        if !positionals.is_empty() {
            positionals.sort_by_key(|(index, _)| *index);
            line.push("--".to_string());
            line.extend(positionals.into_iter().flat_map(|(_, values)| values));
        }
        Ok(line)
    }
}

impl ArgumentSpec {
    /// `value` as command-line words, or why it's the wrong shape.
    fn values(&self, value: &Value) -> Result<Vec<String>, String> {
        let wrong =
            |expected: &str| Err(format!("{} must be {expected}, not {value}", self.property));
        match (self.kind, value) {
            (ValueKind::Switch, Value::Bool(_)) => Ok(Vec::new()),
            (ValueKind::Switch, _) => wrong("true or false"),
            (ValueKind::Integer, Value::Number(number)) if number.is_i64() || number.is_u64() => {
                Ok(vec![number.to_string()])
            }
            (ValueKind::Integer, _) => wrong("a whole number"),
            (ValueKind::Text, Value::String(text)) => Ok(vec![text.clone()]),
            (ValueKind::Text, _) => wrong("a string"),
            (ValueKind::TextList, Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => Ok(text.clone()),
                    _ => Err(format!(
                        "{} must be a list of strings, not {value}",
                        self.property
                    )),
                })
                .collect(),
            (ValueKind::TextList, _) => wrong("a list of strings"),
        }
    }
}
