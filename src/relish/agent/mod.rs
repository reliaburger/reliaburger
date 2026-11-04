//! `relish agent`: commands for LLM agents, with one output contract.
//!
//! Every command here prints one JSON document on stdout, the
//! `relish.agent/v1` [`Envelope`], within a byte budget, and exits with a
//! code that says only whether it answered. None of them is interactive: no
//! prompt, no TUI, no stdin. `-o` is accepted and ignored, because there is
//! only one format.
//!
//! This module holds the contract ([`envelope`], [`budget`]) and the first
//! two commands, `version` and `schema`, which exercise it end to end. The
//! commands that read the cluster and the source come later and reuse it.

pub mod budget;
pub mod envelope;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

pub use budget::{DEFAULT_MAX_BYTES, MIN_MAX_BYTES, Rendered};
pub use envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, ExitStatus, NextStep};

/// `relish agent`'s own flags and its subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct AgentOptions {
    /// Largest answer to print, in bytes (at least 1024). A bigger answer is
    /// trimmed, marked `truncated`, and says how to get the rest.
    #[arg(
        long,
        global = true,
        default_value_t = DEFAULT_MAX_BYTES,
        value_parser = parse_max_bytes,
    )]
    pub max_bytes: usize,

    #[command(subcommand)]
    pub command: AgentCommand,
}

/// The `relish agent` commands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum AgentCommand {
    /// This binary's version, commit, target and cluster formats.
    Version,
    /// The JSON Schema of the envelope and of each command's data.
    Schema {
        /// Only this command's data schema.
        #[arg(value_enum)]
        command: Option<CommandName>,
    },
}

impl AgentCommand {
    /// The name the envelope's `command` field carries.
    pub fn name(&self) -> CommandName {
        match self {
            AgentCommand::Version => CommandName::Version,
            AgentCommand::Schema { .. } => CommandName::Schema,
        }
    }
}

fn parse_max_bytes(value: &str) -> Result<usize, String> {
    let bytes: usize = value
        .parse()
        .map_err(|_| format!("{value:?} isn't a number of bytes"))?;
    if bytes < MIN_MAX_BYTES {
        return Err(format!("{bytes} is below the minimum of {MIN_MAX_BYTES}"));
    }
    Ok(bytes)
}

/// Render `envelope` within `max_bytes`; see [`budget::render`].
pub fn render<T: Serialize>(envelope: &Envelope<T>, max_bytes: usize, rerun: &str) -> Rendered {
    budget::render(envelope, max_bytes, rerun)
}

/// Run one `relish agent` command.
///
/// `rerun` is the command line as [`rerun_command`] rebuilds it, for the
/// hint a trimmed answer carries.
pub fn run(options: &AgentOptions, rerun: &str) -> Rendered {
    let relish = BuildInfo::current();
    match &options.command {
        AgentCommand::Version => render(
            &Envelope::answered(CommandName::Version, relish, version_data()),
            options.max_bytes,
            rerun,
        ),
        AgentCommand::Schema { command } => render(
            &Envelope::answered(CommandName::Schema, relish, schema_data(*command)),
            options.max_bytes,
            rerun,
        ),
    }
}

/// A command that couldn't start, such as one given an invalid endpoint.
pub fn failure(command: CommandName, error: AgentError, max_bytes: usize, rerun: &str) -> Rendered {
    let envelope: Envelope<()> = Envelope::failed(command, BuildInfo::current(), error);
    render(&envelope, max_bytes, rerun)
}

/// `data` for `relish agent version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VersionData {
    /// The Cargo package version.
    pub version: String,
    /// The git commit the binary was built from, when the build knew it.
    pub commit: Option<String>,
    /// The Rust target triple the binary was built for.
    pub target: Option<String>,
    /// The wire and durable-state formats this binary speaks. Nodes with
    /// different numbers can't join the same cluster.
    pub formats: Formats,
}

/// The cluster formats from `src/compatibility.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Formats {
    /// Cluster protocol generation.
    pub protocol: u32,
    /// Durable state generation.
    pub state: u32,
}

fn version_data() -> VersionData {
    VersionData {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: crate::upgrade::version::build_commit().map(str::to_string),
        target: option_env!("RELIABURGER_TARGET").map(str::to_string),
        formats: Formats {
            protocol: crate::compatibility::CURRENT.protocol,
            state: crate::compatibility::CURRENT.state,
        },
    }
}

/// `data` for `relish agent schema`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SchemaData {
    /// The JSON Schema of the envelope, with `data` left open.
    pub envelope: Value,
    /// Each command's `data` schema.
    pub commands: Vec<CommandSchema>,
}

/// One command's `data` schema.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandSchema {
    /// The command.
    pub command: CommandName,
    /// The JSON Schema of its `data`.
    pub data: Value,
}

/// The JSON Schema of `T` as it serialises.
///
/// schemars describes deserialisation by default; these types only ever
/// serialise, and the two can differ (a field with a default is optional on
/// the way in but always present on the way out).
fn schema_of<T: JsonSchema>() -> Value {
    schemars::generate::SchemaSettings::default()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value()
}

/// The `data` schema of `command`, or `None` for `agent` itself, which
/// never answers.
pub fn data_schema(command: CommandName) -> Option<Value> {
    match command {
        CommandName::Agent => None,
        CommandName::Version => Some(schema_of::<VersionData>()),
        CommandName::Schema => Some(schema_of::<SchemaData>()),
    }
}

fn schema_data(only: Option<CommandName>) -> SchemaData {
    let commands = CommandName::ANSWERING
        .iter()
        .copied()
        .filter(|command| only.is_none_or(|only| only == *command))
        .filter_map(|command| data_schema(command).map(|data| CommandSchema { command, data }))
        .collect();
    SchemaData {
        envelope: schema_of::<Envelope<Value>>(),
        commands,
    }
}

/// Global `relish` flags that take a value as the next argument.
const VALUE_FLAGS: &[&str] = &["-o", "--output", "--token", "--ca-cert", "--endpoint"];

/// Whether `args` (without the program name) run a `relish agent` command:
/// the first argument that isn't a flag or a flag's value is `agent`.
pub fn invoked(args: &[String]) -> bool {
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            skip_value = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return arg == "agent";
    }
    false
}

/// The command `args` named after `agent`, or [`CommandName::Agent`] when
/// they named none it knows.
pub fn command_named(args: &[String]) -> CommandName {
    let mut words = args
        .iter()
        .skip_while(|arg| arg.as_str() != "agent")
        .skip(1);
    words
        .find(|arg| !arg.starts_with('-'))
        .and_then(|word| <CommandName as clap::ValueEnum>::from_str(word, false).ok())
        .unwrap_or(CommandName::Agent)
}

/// The `--max-bytes` in `args`, when there is a valid one.
fn max_bytes_in(args: &[String]) -> Option<usize> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--max-bytes" {
            return iter.next().and_then(|value| parse_max_bytes(value).ok());
        }
        if let Some(value) = arg.strip_prefix("--max-bytes=") {
            return parse_max_bytes(value).ok();
        }
    }
    None
}

/// The usage failure for a `relish agent` command line clap rejected.
///
/// `message` is clap's own explanation. It honours a valid `--max-bytes`
/// even though the rest of the line didn't parse.
pub fn usage_failure(args: &[String], message: &str) -> Rendered {
    let error = AgentError::new(ErrorCode::Usage, message.trim_end())
        .with_hint("relish agent --help lists the commands; relish agent schema their output");
    failure(
        command_named(args),
        error,
        max_bytes_in(args).unwrap_or(DEFAULT_MAX_BYTES),
        &rerun_command(args),
    )
}

/// The command line `args` (without the program name) as a hint can quote
/// it: `--max-bytes` dropped, because the hint sets its own, and `--token`
/// dropped, because a secret has no place in the output.
pub fn rerun_command(args: &[String]) -> String {
    let mut words = vec!["relish".to_string()];
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--max-bytes" || arg == "--token" {
            iter.next();
            continue;
        }
        if arg.starts_with("--max-bytes=") || arg.starts_with("--token=") {
            continue;
        }
        words.push(shell_quote(arg));
    }
    words.join(" ")
}

/// `word` as a POSIX shell reads it back unchanged.
fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A stand-in for the `relish` binary's parser, so these tests check
    /// the agent arguments without the rest of the CLI.
    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Debug, clap::Subcommand)]
    enum TestCommand {
        Agent(AgentOptions),
    }

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn parse(line: &str) -> Result<AgentOptions, clap::Error> {
        let mut argv = vec!["relish".to_string()];
        argv.extend(args(line));
        TestCli::try_parse_from(argv).map(|cli| match cli.command {
            TestCommand::Agent(options) => options,
        })
    }

    fn fixture_build() -> BuildInfo {
        BuildInfo {
            version: "0.2.7".to_string(),
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
            dirty: None,
            source_digest: None,
        }
    }

    fn json_of(rendered: &Rendered) -> Value {
        serde_json::from_str(&rendered.json).unwrap()
    }

    #[test]
    fn max_bytes_defaults_to_24_kib_and_goes_after_the_command() {
        assert_eq!(parse("agent version").unwrap().max_bytes, 24 * 1024);
        assert_eq!(
            parse("agent version --max-bytes 4096").unwrap().max_bytes,
            4096
        );
        assert_eq!(
            parse("agent --max-bytes 4096 version").unwrap().max_bytes,
            4096
        );
    }

    #[test]
    fn max_bytes_below_the_minimum_is_a_usage_error() {
        assert!(parse("agent version --max-bytes 100").is_err());
        assert!(parse("agent version --max-bytes lots").is_err());
    }

    #[test]
    fn version_answers_with_this_build() {
        let rendered = run(&parse("agent version").unwrap(), "relish agent version");
        assert_eq!(rendered.exit, ExitStatus::Answered);
        let value = json_of(&rendered);
        assert_eq!(value["schema"], "relish.agent/v1");
        assert_eq!(value["command"], "version");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            value["data"]["formats"]["protocol"],
            crate::compatibility::CURRENT.protocol
        );
        assert_eq!(value["relish"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn schema_lists_the_envelope_and_every_answering_command() {
        let rendered = run(&parse("agent schema").unwrap(), "relish agent schema");
        assert_eq!(rendered.exit, ExitStatus::Answered);
        let value = json_of(&rendered);
        assert_eq!(
            value["truncated"], false,
            "the schema fits the default budget"
        );
        let names: Vec<&str> = value["data"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["command"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["version", "schema"]);
        let envelope = &value["data"]["envelope"];
        let error_codes = &envelope["$defs"]["ErrorCode"];
        assert!(
            error_codes.to_string().contains("tls_untrusted"),
            "{error_codes}"
        );
    }

    #[test]
    fn schema_for_one_command_lists_only_that_one() {
        let rendered = run(
            &parse("agent schema version").unwrap(),
            "relish agent schema version",
        );
        let value = json_of(&rendered);
        let commands = value["data"]["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command"], "version");
        assert!(
            commands[0]["data"]["properties"]["formats"].is_object(),
            "{}",
            commands[0]["data"]
        );
    }

    #[test]
    fn schema_does_not_offer_agent_itself() {
        assert!(parse("agent schema agent").is_err());
    }

    #[test]
    fn agent_invocations_are_recognised_past_global_flags() {
        assert!(invoked(&args("agent version")));
        assert!(invoked(&args("--endpoint https://x:9117 agent version")));
        assert!(invoked(&args("-o json agent")));
        assert!(!invoked(&args("status")));
        assert!(!invoked(&args("--token agent status")));
        assert!(!invoked(&args("logs agent")));
        assert!(!invoked(&[]));
    }

    #[test]
    fn the_command_name_comes_from_the_word_after_agent() {
        assert_eq!(
            command_named(&args("agent version --bogus")),
            CommandName::Version
        );
        assert_eq!(
            command_named(&args("agent --max-bytes 2048 schema")),
            CommandName::Agent,
            "a flag's value isn't skipped, so this one falls back to agent"
        );
        assert_eq!(command_named(&args("agent nonsense")), CommandName::Agent);
        assert_eq!(command_named(&args("agent")), CommandName::Agent);
    }

    #[test]
    fn rerun_drops_the_budget_and_the_token_and_quotes_the_rest() {
        let line: Vec<String> = [
            "--token",
            "secret-1",
            "agent",
            "version",
            "--max-bytes",
            "2048",
            "--token=secret-2",
            "--max-bytes=4096",
            "it's here",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        let rerun = rerun_command(&line);
        assert_eq!(rerun, r"relish agent version 'it'\''s here'");
        assert!(!rerun.contains("secret"));
    }

    #[test]
    fn a_usage_failure_is_an_envelope_with_exit_2() {
        let line = args("agent version --bogus");
        let rendered = usage_failure(&line, "error: unexpected argument '--bogus' found\n");
        assert_eq!(rendered.exit, ExitStatus::Usage);
        assert_eq!(rendered.exit.code(), 2);
        let value = json_of(&rendered);
        assert_eq!(value["ok"], false);
        assert_eq!(value["command"], "version");
        assert_eq!(value["error"]["code"], "usage");
        assert_eq!(
            value["error"]["message"],
            "error: unexpected argument '--bogus' found"
        );
    }

    #[test]
    fn rendering_takes_the_exit_status_from_what_was_printed() {
        let unreachable: Envelope<()> = Envelope::failed(
            CommandName::Version,
            fixture_build(),
            AgentError::from(&crate::relish::RelishError::AgentUnreachable),
        );
        assert_eq!(
            render(&unreachable, 4096, "relish agent version")
                .exit
                .code(),
            3
        );
        let forbidden: Envelope<()> = Envelope::failed(
            CommandName::Version,
            fixture_build(),
            AgentError::new(ErrorCode::Forbidden, "read-only token"),
        );
        assert_eq!(
            render(&forbidden, 4096, "relish agent version").exit.code(),
            4
        );
        let internal: Envelope<()> = Envelope::failed(
            CommandName::Version,
            fixture_build(),
            AgentError::new(ErrorCode::Internal, "bug"),
        );
        assert_eq!(
            render(&internal, 4096, "relish agent version").exit.code(),
            5
        );
    }

    // The snapshots pin each shape of the envelope. A change to any of them
    // is a change to the contract every agent and `relish mcp` read.

    #[test]
    fn snapshot_of_an_answer() {
        let envelope = Envelope::answered(
            CommandName::Version,
            fixture_build(),
            VersionData {
                version: "0.2.7".to_string(),
                commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
                target: Some("aarch64-apple-darwin".to_string()),
                formats: Formats {
                    protocol: 57,
                    state: 74,
                },
            },
        )
        .with_next(NextStep {
            run: "relish agent schema version".to_string(),
            why: "the shape of this answer".to_string(),
            read_only: true,
        });
        let rendered = render(&envelope, DEFAULT_MAX_BYTES, "relish agent version");
        insta::assert_json_snapshot!(json_of(&rendered));
    }

    #[test]
    fn snapshot_of_a_truncated_answer() {
        let items: Vec<Value> = (0..200)
            .map(|index| serde_json::json!({"app": format!("app-{index:03}")}))
            .collect();
        let envelope = Envelope::answered(
            CommandName::Version,
            fixture_build(),
            serde_json::json!({"apps": items}),
        );
        let rendered = render(&envelope, MIN_MAX_BYTES, "relish agent version");
        assert!(rendered.json.len() <= MIN_MAX_BYTES);
        insta::assert_json_snapshot!(json_of(&rendered));
    }

    #[test]
    fn snapshot_of_each_error_class() {
        let failures = [
            AgentError::new(
                ErrorCode::Usage,
                "error: unexpected argument '--bogus' found",
            )
            .with_hint("relish agent --help lists the commands; relish agent schema their output"),
            AgentError::from(&crate::relish::RelishError::AgentUnreachable),
            AgentError::from(&crate::relish::RelishError::ApiError {
                status: 401,
                body: "missing bearer token".to_string(),
            }),
            AgentError::from(&crate::relish::RelishError::ApiError {
                status: 403,
                body: "token role read-only can't do this".to_string(),
            }),
            AgentError::from(&crate::relish::RelishError::Io(std::io::Error::other(
                "disk on fire",
            ))),
        ];
        let rendered: Vec<Value> = failures
            .into_iter()
            .map(|error| {
                let envelope: Envelope<()> =
                    Envelope::failed(CommandName::Version, fixture_build(), error);
                let rendered = render(&envelope, DEFAULT_MAX_BYTES, "relish agent version");
                serde_json::json!({"exit": rendered.exit.code(), "stdout": json_of(&rendered)})
            })
            .collect();
        insta::assert_json_snapshot!(rendered);
    }

    #[test]
    fn snapshot_of_the_envelope_schema() {
        insta::assert_json_snapshot!(schema_of::<Envelope<Value>>());
    }
}
