//! `relish agent`: commands for LLM agents, with one output contract.
//!
//! Every command here prints one JSON document on stdout, the
//! `relish.agent/v1` [`Envelope`], within a byte budget, and exits with a
//! code that says only whether it answered. None of them is interactive: no
//! prompt, no TUI, no stdin. `-o` is accepted and ignored, because there is
//! only one format.
//!
//! This module holds the contract ([`envelope`], [`budget`], and
//! [`untrusted`] for text a workload wrote), the first two commands,
//! `version` and `schema`, which exercise it end to end, [`source`], which
//! reads the source this binary was built from, [`logs`], which reads an
//! app's recent lines, and [`manual`], which searches and prints the manual.
//! [`context`] and [`diagnose`] read the cluster through `relish wtf`'s
//! collector, and `explain` says what each of `wtf`'s findings means.
//! [`guide`] prints the manual's agent chapter.

pub mod budget;
pub mod context;
pub mod diagnose;
pub mod envelope;
pub mod fit;
pub mod guide;
pub mod lease;
pub mod logs;
pub mod manual;
pub mod patch;
pub mod report;
pub mod setup;
pub mod source;
pub mod untrusted;
pub mod upgrade_check;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

pub use budget::{DEFAULT_MAX_BYTES, GUIDE_MAX_BYTES, MIN_MAX_BYTES, Rendered, default_max_bytes};
pub use envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, ExitStatus, NextStep};

/// `relish agent`'s own flags and its subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct AgentOptions {
    /// Largest answer to print, in bytes (at least 1024; by default 24576,
    /// and 65536 for `guide`). A bigger answer is trimmed, marked
    /// `truncated`, and says how to get the rest.
    // No clap default: the default depends on the command, so `None` means
    // "not given" and [`AgentOptions::max_bytes`] picks the command's.
    #[arg(long, global = true, value_parser = parse_max_bytes)]
    pub max_bytes: Option<usize>,

    #[command(subcommand)]
    pub command: AgentCommand,
}

/// `relish agent` as clap defines it, without the rest of `relish`: the
/// tree `relish agent guide` lists and `relish mcp` turns into tools.
pub fn agent_command() -> clap::Command {
    // `Args` is the trait `#[derive(clap::Args)]` implemented for
    // `AgentOptions`; `augment_args` adds its flags and subcommands to a
    // command we name ourselves.
    <AgentOptions as clap::Args>::augment_args(clap::Command::new("agent"))
}

/// The `relish agent` commands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum AgentCommand {
    /// How to use relish as an agent: the manual chapter, as Markdown sections.
    Guide,
    /// This binary's version, commit, target and cluster formats.
    Version,
    /// The JSON Schema of the envelope and of each command's data.
    Schema {
        /// Only this command's data schema.
        #[arg(value_enum)]
        command: Option<CommandName>,
    },
    /// The source this binary was built from: search it, read it, extract it.
    Source(source::SourceOptions),
    /// An app's recent log lines, newest kept, each wrapped as untrusted.
    Logs(logs::LogsArgs),
    /// The manual compiled into this binary: search it, show a section.
    Manual {
        #[command(subcommand)]
        command: manual::ManualCommand,
    },
    /// The first call: findings, unknowns, faults, alerts, one line per app
    /// and the newest events, in one bounded answer.
    Context(context::ContextArgs),
    /// Every finding with its evidence, read-only next steps, and pointers
    /// into the manual and the source.
    Diagnose(diagnose::DiagnoseArgs),
    /// What a finding means, its thresholds and where it's decided; every
    /// finding when no id is given. Reads only this binary.
    Explain {
        /// A finding id, e.g. `crashloop`.
        id: Option<String>,
    },
    /// Whether the cluster can take the latest release: `ready`,
    /// `up_to_date`, `needs_fresh_cluster`, `blocked` or `unknown`.
    UpgradeCheck(upgrade_check::UpgradeCheckArgs),
    /// Draft a redacted bug report for a human to post. Never posts.
    Report(Box<report::ReportArgs>),
    /// Build, test and try a fix on a throwaway cluster.
    Patch(patch::PatchOptions),
    /// Wire Reliaburger into the AI agents on this machine. Shows the diff;
    /// writes only with --yes.
    Setup(setup::SetupOptions),
    /// Take, renew or release a lease on an app or a namespace.
    Lease(lease::LeaseOptions),
}

impl AgentOptions {
    /// The budget for this command line: `--max-bytes` when given, else the
    /// command's default ([`default_max_bytes`]).
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
            .unwrap_or_else(|| default_max_bytes(self.command.name()))
    }
}

impl AgentCommand {
    /// The name the envelope's `command` field carries.
    pub fn name(&self) -> CommandName {
        match self {
            AgentCommand::Guide => CommandName::Guide,
            AgentCommand::Version => CommandName::Version,
            AgentCommand::Schema { .. } => CommandName::Schema,
            AgentCommand::Source(options) => options.command.name(),
            AgentCommand::Logs(_) => CommandName::Logs,
            AgentCommand::Manual { command } => command.name(),
            AgentCommand::Context(_) => CommandName::Context,
            AgentCommand::Diagnose(_) => CommandName::Diagnose,
            AgentCommand::Explain { .. } => CommandName::Explain,
            AgentCommand::UpgradeCheck(_) => CommandName::UpgradeCheck,
            AgentCommand::Report(_) => CommandName::Report,
            AgentCommand::Patch(options) => options.command.name(),
            AgentCommand::Setup(_) => CommandName::Setup,
            AgentCommand::Lease(options) => options.command.name(),
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
pub async fn run(options: &AgentOptions, rerun: &str) -> Rendered {
    let max_bytes = options.max_bytes();
    match &options.command {
        AgentCommand::Guide => guide::run(max_bytes, rerun),
        AgentCommand::Version => render(
            &Envelope::answered(CommandName::Version, BuildInfo::current(), version_data()),
            max_bytes,
            rerun,
        ),
        AgentCommand::Schema { command } => render(
            &Envelope::answered(
                CommandName::Schema,
                BuildInfo::current(),
                schema_data(*command),
            ),
            max_bytes,
            rerun,
        ),
        AgentCommand::Source(source) => source::run(source, max_bytes, rerun).await,
        AgentCommand::Logs(args) => logs::run(args, max_bytes, rerun).await,
        AgentCommand::Manual { command } => manual::run(command, max_bytes, rerun),
        AgentCommand::Context(args) => context::run(args, max_bytes, rerun).await,
        AgentCommand::Diagnose(args) => diagnose::run_diagnose(args, max_bytes, rerun).await,
        AgentCommand::Explain { id } => diagnose::run_explain(id.as_deref(), max_bytes, rerun),
        AgentCommand::UpgradeCheck(args) => upgrade_check::run(args, max_bytes, rerun).await,
        AgentCommand::Report(args) => report::run(args, max_bytes, rerun),
        AgentCommand::Patch(patch) => patch::run(patch, max_bytes, rerun).await,
        AgentCommand::Setup(setup) => setup::run(setup, max_bytes, rerun).await,
        AgentCommand::Lease(lease) => lease::run(lease, max_bytes, rerun).await,
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
        CommandName::Guide => Some(schema_of::<guide::GuideData>()),
        CommandName::Version => Some(schema_of::<VersionData>()),
        CommandName::Schema => Some(schema_of::<SchemaData>()),
        CommandName::SourceVersion => Some(schema_of::<source::VersionData>()),
        CommandName::SourceTree => Some(schema_of::<source::TreeData>()),
        CommandName::SourceGrep => Some(schema_of::<source::GrepData>()),
        CommandName::SourceShow => Some(schema_of::<source::ShowData>()),
        CommandName::SourceExtract => Some(schema_of::<source::ExtractData>()),
        CommandName::Logs => Some(schema_of::<logs::LogsData>()),
        CommandName::ManualSearch => Some(schema_of::<manual::SearchData>()),
        CommandName::ManualShow => Some(schema_of::<manual::ShowData>()),
        CommandName::Context => Some(schema_of::<context::ContextData>()),
        CommandName::Diagnose => Some(schema_of::<diagnose::DiagnoseData>()),
        CommandName::Explain => Some(schema_of::<diagnose::ExplainData>()),
        CommandName::UpgradeCheck => Some(schema_of::<upgrade_check::UpgradeCheckData>()),
        CommandName::Report => Some(schema_of::<report::ReportData>()),
        CommandName::PatchInit => Some(schema_of::<patch::InitData>()),
        CommandName::PatchBuild => Some(schema_of::<patch::BuildData>()),
        CommandName::PatchTest => Some(schema_of::<patch::TestData>()),
        CommandName::PatchTry => Some(schema_of::<patch::TryData>()),
        CommandName::PatchDiff => Some(schema_of::<patch::DiffData>()),
        CommandName::PatchApply => Some(schema_of::<patch::ApplyData>()),
        CommandName::PatchDestroy => Some(schema_of::<patch::DestroyData>()),
        CommandName::Setup => Some(schema_of::<setup::SetupData>()),
        CommandName::LeaseTake | CommandName::LeaseRenew | CommandName::LeaseRelease => {
            Some(schema_of::<lease::LeaseData>())
        }
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
pub(crate) const VALUE_FLAGS: &[&str] = &["-o", "--output", "--token", "--ca-cert", "--endpoint"];

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
/// they named none it knows. `source`, `manual` and `patch` take the word
/// after them too, as in `source grep`.
pub fn command_named(args: &[String]) -> CommandName {
    let mut words = args
        .iter()
        .skip_while(|arg| arg.as_str() != "agent")
        .skip(1)
        .filter(|arg| !arg.starts_with('-'));
    let name = match words.next().map(String::as_str) {
        Some(family @ ("source" | "manual" | "patch")) => match words.next() {
            Some(word) => format!("{family}-{word}"),
            None => return CommandName::Agent,
        },
        Some(word) => word.to_string(),
        None => return CommandName::Agent,
    };
    <CommandName as clap::ValueEnum>::from_str(&name, false).unwrap_or(CommandName::Agent)
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
/// `message` is clap's own explanation and `lead`, when there is one, opens
/// the hint: a read-only command close to what was typed, or where to
/// start. It honours a valid `--max-bytes`
/// even though the rest of the line didn't parse.
pub fn usage_failure(args: &[String], message: &str, lead: Option<&str>) -> Rendered {
    let help = "relish agent --help lists the commands; relish agent schema their output";
    let hint = match lead {
        Some(lead) => format!("{lead} {help}"),
        None => help.to_string(),
    };
    let error = AgentError::new(ErrorCode::Usage, message.trim_end()).with_hint(hint);
    let command = command_named(args);
    failure(
        command,
        error,
        max_bytes_in(args).unwrap_or_else(|| default_max_bytes(command)),
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
pub(crate) fn shell_quote(word: &str) -> String {
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
        assert_eq!(parse("agent version").unwrap().max_bytes(), 24 * 1024);
        assert_eq!(
            parse("agent version --max-bytes 4096").unwrap().max_bytes(),
            4096
        );
        assert_eq!(
            parse("agent --max-bytes 4096 version").unwrap().max_bytes(),
            4096
        );
    }

    #[test]
    fn guide_defaults_to_64_kib_and_every_other_command_to_24_kib() {
        use clap::ValueEnum;
        for &command in CommandName::value_variants() {
            let expected = match command {
                CommandName::Guide => 64 * 1024,
                _ => 24 * 1024,
            };
            assert_eq!(default_max_bytes(command), expected, "{command:?}");
        }
        assert_eq!(default_max_bytes(CommandName::Agent), 24 * 1024);
        assert_eq!(parse("agent guide").unwrap().max_bytes(), 64 * 1024);
        for line in [
            "agent version",
            "agent schema",
            "agent explain",
            "agent context",
            "agent logs payments",
            "agent manual search deploy",
            "agent source version",
        ] {
            assert_eq!(parse(line).unwrap().max_bytes(), 24 * 1024, "{line}");
        }
    }

    #[test]
    fn an_explicit_max_bytes_beats_the_guides_default() {
        assert_eq!(
            parse("agent guide --max-bytes 4096").unwrap().max_bytes(),
            4096
        );
        assert_eq!(
            parse("agent --max-bytes 131072 guide").unwrap().max_bytes(),
            128 * 1024
        );
    }

    #[test]
    fn max_bytes_below_the_minimum_is_a_usage_error() {
        assert!(parse("agent version --max-bytes 100").is_err());
        assert!(parse("agent version --max-bytes lots").is_err());
    }

    #[tokio::test]
    async fn version_answers_with_this_build() {
        let rendered = run(&parse("agent version").unwrap(), "relish agent version").await;
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
        let digest = crate::relish::source_bundle::digest().unwrap();
        assert_eq!(value["relish"]["source_digest"], format!("sha256:{digest}"));
        assert_eq!(
            value["relish"]["dirty"],
            serde_json::json!(crate::relish::source_bundle::dirty())
        );
    }

    #[tokio::test]
    async fn schema_lists_the_envelope_and_every_answering_command() {
        // Every command's schema together outgrew the default budget once
        // `context` and `diagnose` arrived; `schema <command>` is the
        // bounded call, and the test below holds each one to the default.
        let rendered = run(
            &parse("agent schema --max-bytes 131072").unwrap(),
            "relish agent schema",
        )
        .await;
        assert_eq!(rendered.exit, ExitStatus::Answered);
        let value = json_of(&rendered);
        assert_eq!(value["truncated"], false, "the whole schema fits 128 KiB");
        let names: Vec<&str> = value["data"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["command"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "guide",
                "version",
                "schema",
                "source version",
                "source tree",
                "source grep",
                "source show",
                "source extract",
                "logs",
                "manual search",
                "manual show",
                "context",
                "diagnose",
                "explain",
                "upgrade-check",
                "report",
                "patch init",
                "patch build",
                "patch test",
                "patch try",
                "patch diff",
                "patch apply",
                "patch destroy",
                "setup",
                "lease take",
                "lease renew",
                "lease release"
            ]
        );
        let envelope = &value["data"]["envelope"];
        let error_codes = &envelope["$defs"]["ErrorCode"];
        assert!(
            error_codes.to_string().contains("tls_untrusted"),
            "{error_codes}"
        );
    }

    #[tokio::test]
    async fn each_commands_schema_fits_the_default_budget() {
        for command in CommandName::ANSWERING {
            let line = format!(
                "agent schema {}",
                clap::ValueEnum::to_possible_value(command)
                    .unwrap()
                    .get_name()
            );
            let rendered = run(&parse(&line).unwrap(), &format!("relish {line}")).await;
            assert_eq!(json_of(&rendered)["truncated"], false, "{line}");
        }
    }

    #[tokio::test]
    async fn schema_for_one_command_lists_only_that_one() {
        let rendered = run(
            &parse("agent schema version").unwrap(),
            "relish agent schema version",
        )
        .await;
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

    #[tokio::test]
    async fn schema_names_a_source_command_with_a_hyphen() {
        let rendered = run(
            &parse("agent schema source-grep").unwrap(),
            "relish agent schema source-grep",
        )
        .await;
        let value = json_of(&rendered);
        let commands = value["data"]["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command"], "source grep");
        let properties = &commands[0]["data"]["properties"];
        for key in ["files_searched", "total_matches", "matches"] {
            assert!(properties[key].is_object(), "{key}: {properties}");
        }
    }

    #[test]
    fn source_commands_parse_with_their_flags() {
        let options =
            parse("agent source grep fn.main --path src/** --context 4 -i --offline").unwrap();
        let AgentCommand::Source(source) = &options.command else {
            panic!("{options:?}");
        };
        assert!(source.offline);
        let source::SourceCommand::Grep(grep) = &source.command else {
            panic!("{source:?}");
        };
        assert_eq!(grep.pattern, "fn.main");
        assert_eq!(grep.path.as_deref(), Some("src/**"));
        assert_eq!(grep.context, 4);
        assert_eq!(grep.max_matches, 50);
        assert!(grep.ignore_case);
        assert_eq!(options.command.name(), CommandName::SourceGrep);
        assert!(parse("agent source tree src --depth 0").is_err());
        assert!(parse("agent source grep x --context 99").is_err());
        assert!(
            parse("agent --offline source version").is_err(),
            "--offline belongs to source"
        );
    }

    #[test]
    fn logs_and_manual_commands_parse_with_their_flags() {
        let options = parse("agent logs payments --since 1h --tail 50 --stream stderr").unwrap();
        let AgentCommand::Logs(logs) = &options.command else {
            panic!("{options:?}");
        };
        assert_eq!(logs.app, "payments");
        assert_eq!(logs.since, "1h");
        assert_eq!(logs.tail, 50);
        assert_eq!(logs.stream, Some(logs::StreamArg::Stderr));
        assert_eq!(options.command.name(), CommandName::Logs);
        let options = parse("agent manual search rolling deploy --max-bytes 4096").unwrap();
        assert_eq!(options.max_bytes(), 4096);
        let AgentCommand::Manual {
            command: manual::ManualCommand::Search { words },
        } = &options.command
        else {
            panic!("{options:?}");
        };
        assert_eq!(words, &["rolling", "deploy"]);
        assert_eq!(
            parse("agent manual show diagnostics#reading-wtf")
                .unwrap()
                .command
                .name(),
            CommandName::ManualShow
        );
        assert!(parse("agent manual search").is_err(), "search needs a word");
        assert!(parse("agent manual").is_err());
    }

    #[test]
    fn cluster_commands_parse_with_their_flags() {
        let options = parse("agent context --events 50").unwrap();
        let AgentCommand::Context(context) = &options.command else {
            panic!("{options:?}");
        };
        assert_eq!(context.events, 50);
        assert_eq!(options.command.name(), CommandName::Context);
        assert!(parse("agent context --events 0").is_err());
        assert!(parse("agent context --events 201").is_err());
        let options = parse("agent diagnose --app web").unwrap();
        let AgentCommand::Diagnose(diagnose) = &options.command else {
            panic!("{options:?}");
        };
        assert_eq!(diagnose.app.as_deref(), Some("web"));
        let options = parse("agent explain crashloop").unwrap();
        assert!(
            matches!(&options.command, AgentCommand::Explain { id: Some(id) } if id == "crashloop")
        );
        assert!(matches!(
            parse("agent explain").unwrap().command,
            AgentCommand::Explain { id: None }
        ));
        assert_eq!(
            command_named(&args("agent explain --bogus")),
            CommandName::Explain
        );
    }

    #[test]
    fn patch_commands_parse_with_their_flags() {
        let options = parse("agent patch build ./w --bun --allow-build-changes").unwrap();
        let AgentCommand::Patch(patch) = &options.command else {
            panic!("{options:?}");
        };
        assert!(matches!(
            &patch.command,
            patch::PatchCommand::Build {
                relish: false,
                bun: true,
                allow_build_changes: true,
                ..
            }
        ));
        assert_eq!(options.command.name(), CommandName::PatchBuild);
        assert!(parse("agent patch try ./w --nodes 3").is_ok());
        assert!(parse("agent patch try ./w --nodes 2").is_err());
        assert!(parse("agent patch destroy --all").is_ok());
        // An inline diff starts with `---`, which isn't a flag.
        let options = parse("agent patch apply ./w --- --check").unwrap();
        let AgentCommand::Patch(patch) = &options.command else {
            panic!("not patch: {options:?}");
        };
        assert!(
            matches!(
                &patch.command,
                patch::PatchCommand::Apply { diff, check: true, .. } if diff == "---"
            ),
            "{patch:?}"
        );
        assert!(
            parse("agent patch destroy").is_err(),
            "a workspace or --all"
        );
        assert!(parse("agent patch destroy ./w --all").is_err());
        assert_eq!(
            command_named(&args("agent patch test --bogus")),
            CommandName::PatchTest
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
        assert_eq!(
            command_named(&args("agent source grep --bogus")),
            CommandName::SourceGrep
        );
        assert_eq!(command_named(&args("agent source")), CommandName::Agent);
        assert_eq!(
            command_named(&args("agent source nope")),
            CommandName::Agent
        );
        assert_eq!(
            command_named(&args("agent manual show diagnostics --bogus")),
            CommandName::ManualShow
        );
        assert_eq!(
            command_named(&args("agent logs payments --bogus")),
            CommandName::Logs
        );
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
        let rendered = usage_failure(&line, "error: unexpected argument '--bogus' found\n", None);
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
            AgentError::from(&crate::relish::RelishError::AgentUnreachable {
                endpoint: "https://10.0.0.5:9117".to_string(),
                cause: crate::relish::UnreachableCause::Refused,
            }),
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
            AgentError::from(&crate::relish::RelishError::AgentUnreachable {
                endpoint: "https://10.0.0.5:9117".to_string(),
                cause: crate::relish::UnreachableCause::Refused,
            }),
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
