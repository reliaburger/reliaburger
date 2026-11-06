//! `relish agent setup`: wire Reliaburger into the agents on this machine.
//!
//! For each agent it finds (Claude Code, Codex, Cursor, VS Code, Goose,
//! Gemini CLI), setup adds one MCP server entry, `reliaburger`, that runs
//! `relish mcp`, and for Claude Code the Reliaburger skill. It is the one
//! `relish agent` command that writes outside the cluster, so it is careful
//! in five ways, each with a test:
//!
//! - Without `--yes` it writes nothing: the answer is the exact diff of
//!   every file it would change. `--dry-run` (or `--print`) says so
//!   explicitly. A human reads that, then runs it again with `--yes`.
//! - It writes only the files its answer names, and inside them only its own
//!   entry.
//! - An entry called `reliaburger` that isn't exactly ours is someone
//!   else's: setup reports a `conflict` and leaves it, unless `--force`.
//! - Running it twice changes nothing the second time.
//! - No entry approves anything: no `trust`, no auto-approve, no wildcard.
//!
//! `--remove` takes the entries (and the skill) back out, by the same rules.

pub mod clients;
pub mod edit;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use super::budget::{self, Rendered};
use super::envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, NextStep};
use clients::{Client, Home};
use edit::Existing;

pub use clients::{MCP_COMMAND_LINE, MCP_DESCRIPTION, MCP_SERVER_NAME, mcp_command_line};

/// The Reliaburger skill for agents that read Agent Skills, as `make
/// agent-docs` generates it from the manual's agent chapter. Setup installs
/// it for Claude Code.
pub const SKILL_MD: &str =
    include_str!("../../../../plugins/reliaburger/skills/reliaburger/SKILL.md");

/// `relish agent setup`'s arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct SetupOptions {
    /// Only these agents, found or not. Without any, every agent found on
    /// this machine.
    #[arg(value_enum)]
    pub agents: Vec<Client>,

    /// Write the changes. Without it, setup only shows them.
    #[arg(long, conflicts_with = "dry_run")]
    pub yes: bool,

    /// Show the exact changes and write nothing, which is also what setup
    /// does without --yes.
    #[arg(long, visible_alias = "print")]
    pub dry_run: bool,

    /// Take Reliaburger back out of the agents instead.
    #[arg(long)]
    pub remove: bool,

    /// Replace, or remove, a `reliaburger` entry that setup didn't write.
    #[arg(long)]
    pub force: bool,
}

/// `data` for `relish agent setup`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetupData {
    /// Whether setup was adding Reliaburger or removing it.
    pub action: Action,
    /// Whether setup wrote anything. `false` without `--yes`: then every
    /// change below is only what it would do.
    pub applied: bool,
    /// The MCP server every entry names.
    pub server: Server,
    /// Every agent setup knows, found or not, in a fixed order.
    pub agents: Vec<AgentPlan>,
}

/// Adding or removing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// `relish agent setup`.
    Install,
    /// `relish agent setup --remove`.
    Remove,
}

/// The MCP server setup registers.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Server {
    /// The entry's name in every agent's config.
    pub name: String,
    /// The command line the agent runs, over stdio.
    pub command: Vec<String>,
}

/// One agent: whether it's here, and what setup changes for it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentPlan {
    /// The agent.
    pub agent: Client,
    /// Whether its config directory exists on this machine.
    pub detected: bool,
    /// What setup changes (or would change) for it. Empty for an agent that
    /// wasn't found or named.
    pub changes: Vec<Change>,
}

/// One file setup changes, or would.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Change {
    /// An MCP server entry, or a skill file.
    pub kind: ChangeKind,
    /// The file, absolute. Setup writes no other.
    pub path: PathBuf,
    /// Where in the file: `mcpServers.reliaburger`, or `file` for a skill.
    pub entry: String,
    /// What happens to it.
    pub status: ChangeStatus,
    /// Whether setup wrote it this time.
    pub written: bool,
    /// The change as a unified diff with no context lines, so it shows only
    /// what setup adds or takes away. `null` when nothing changes.
    pub diff: Option<String>,
    /// Why setup left it alone, for `conflict` and `unreadable`, or why a
    /// write failed.
    pub note: Option<String>,
}

/// What a change is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The `reliaburger` entry in the agent's MCP config.
    McpServer,
    /// The Reliaburger skill.
    Skill,
}

/// What happens to one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    /// It isn't there; setup adds it.
    Add,
    /// Someone else's entry, replaced because of `--force`.
    Replace,
    /// Setup takes it out.
    Remove,
    /// Already as it should be; nothing to do.
    Unchanged,
    /// An entry by that name that setup didn't write; left alone.
    Conflict,
    /// The file doesn't parse, or can't be rewritten safely; left alone.
    Unreadable,
}

impl ChangeStatus {
    fn changes_something(self) -> bool {
        matches!(
            self,
            ChangeStatus::Add | ChangeStatus::Replace | ChangeStatus::Remove
        )
    }
}

/// A planned change, with the text to write.
#[derive(Debug, Clone)]
struct Planned {
    change: Change,
    /// The file as setup read it; `None` when it didn't exist.
    before: Option<String>,
    /// What to write; `None` deletes the file (a skill being removed).
    after: Option<String>,
}

/// Work out, and with `--yes` make, every change.
///
/// `skill` is the skill file's text, when this build carries one.
pub fn setup(options: &SetupOptions, home: &Home, skill: Option<&str>) -> SetupData {
    let action = if options.remove {
        Action::Remove
    } else {
        Action::Install
    };
    let mut agents = Vec::new();
    for &client in Client::ALL {
        let detected = client.detected(home);
        let chosen = if options.agents.is_empty() {
            detected
        } else {
            options.agents.contains(&client)
        };
        let mut changes = Vec::new();
        if chosen {
            changes.push(plan_server(client, home, action, options.force));
            if let (Some(path), Some(skill)) = (client.skill(home), skill) {
                changes.push(plan_skill(path, skill, action, options.force));
            }
        }
        if options.yes {
            for planned in &mut changes {
                apply(planned);
            }
        }
        agents.push(AgentPlan {
            agent: client,
            detected,
            changes: changes.into_iter().map(|planned| planned.change).collect(),
        });
    }
    SetupData {
        action,
        applied: options.yes,
        server: Server {
            name: MCP_SERVER_NAME.to_string(),
            command: mcp_command_line(),
        },
        agents,
    }
}

/// The file setup should write for `path`: the file a symlink points at,
/// so a dotfiles link stays a link.
fn resolve(path: PathBuf) -> PathBuf {
    let is_link = fs::symlink_metadata(&path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        fs::canonicalize(&path).unwrap_or(path)
    } else {
        path
    }
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn change(kind: ChangeKind, path: PathBuf, entry: String, status: ChangeStatus) -> Change {
    Change {
        kind,
        path,
        entry,
        status,
        written: false,
        diff: None,
        note: None,
    }
}

fn left_alone(mut change: Change, status: ChangeStatus, note: String) -> Planned {
    change.status = status;
    change.note = Some(note);
    Planned {
        change,
        before: None,
        after: None,
    }
}

const CONFLICT_NOTE: &str = "an entry by this name that setup didn't write; read it, then add --force to replace or remove it";

fn plan_server(client: Client, home: &Home, action: Action, force: bool) -> Planned {
    let config = client.config(home);
    let path = resolve(config.path);
    let entry_name = format!("{}.{MCP_SERVER_NAME}", config.parent);
    let base = change(
        ChangeKind::McpServer,
        path.clone(),
        entry_name,
        ChangeStatus::Unchanged,
    );
    let before = match read_optional(&path) {
        Ok(before) => before,
        Err(error) => {
            return left_alone(
                base,
                ChangeStatus::Unreadable,
                format!("can't read it: {error}"),
            );
        }
    };
    let text = before.as_deref().unwrap_or("");
    let ours = client.entry();
    let existing = match edit::existing(config.format, text, config.parent, MCP_SERVER_NAME, &ours)
    {
        Ok(existing) => existing,
        Err(error) => {
            return left_alone(
                base,
                ChangeStatus::Unreadable,
                unreadable_note(&error, &ours),
            );
        }
    };
    let status = match (action, existing, force) {
        (Action::Install, Existing::Absent, _) => ChangeStatus::Add,
        (Action::Install, Existing::Same, _) | (Action::Remove, Existing::Absent, _) => {
            ChangeStatus::Unchanged
        }
        (Action::Install, Existing::Different, true) => ChangeStatus::Replace,
        (Action::Remove, Existing::Same, _) | (Action::Remove, Existing::Different, true) => {
            ChangeStatus::Remove
        }
        (_, Existing::Different, false) => {
            return left_alone(base, ChangeStatus::Conflict, CONFLICT_NOTE.to_string());
        }
    };
    if !status.changes_something() {
        return Planned {
            change: base,
            before,
            after: None,
        };
    }
    let after = match action {
        Action::Install => edit::insert(config.format, text, config.parent, MCP_SERVER_NAME, &ours),
        Action::Remove => edit::remove(config.format, text, config.parent, MCP_SERVER_NAME),
    };
    match after {
        Ok(after) => planned(base, status, before, Some(after)),
        Err(error) => left_alone(
            base,
            ChangeStatus::Unreadable,
            unreadable_note(&error, &ours),
        ),
    }
}

/// Why a config was left alone, with the entry to add by hand.
fn unreadable_note(error: &edit::EditError, ours: &serde_json::Value) -> String {
    format!(
        "{error}; setup left it alone. To add it by hand, put this under {MCP_SERVER_NAME:?}: {ours}"
    )
}

fn plan_skill(path: PathBuf, skill: &str, action: Action, force: bool) -> Planned {
    let path = resolve(path);
    let base = change(
        ChangeKind::Skill,
        path.clone(),
        "file".to_string(),
        ChangeStatus::Unchanged,
    );
    let before = match read_optional(&path) {
        Ok(before) => before,
        Err(error) => {
            return left_alone(
                base,
                ChangeStatus::Unreadable,
                format!("can't read it: {error}"),
            );
        }
    };
    let ours = before.as_deref() == Some(skill);
    let status = match (action, before.is_some(), ours, force) {
        (Action::Install, false, _, _) => ChangeStatus::Add,
        (Action::Install, true, true, _) | (Action::Remove, false, _, _) => ChangeStatus::Unchanged,
        (Action::Install, true, false, true) => ChangeStatus::Replace,
        (Action::Remove, true, true, _) | (Action::Remove, true, false, true) => {
            ChangeStatus::Remove
        }
        (_, true, false, false) => {
            return left_alone(base, ChangeStatus::Conflict, CONFLICT_NOTE.to_string());
        }
    };
    let after = match status {
        ChangeStatus::Add | ChangeStatus::Replace => Some(skill.to_string()),
        _ => None,
    };
    if !status.changes_something() {
        return Planned {
            change: base,
            before,
            after,
        };
    }
    planned(base, status, before, after)
}

fn planned(
    mut change: Change,
    status: ChangeStatus,
    before: Option<String>,
    after: Option<String>,
) -> Planned {
    change.status = status;
    change.diff = Some(diff(
        &change.path,
        before.as_deref().unwrap_or(""),
        after.as_deref().unwrap_or(""),
    ));
    Planned {
        change,
        before,
        after,
    }
}

/// A unified diff with no context lines, redacted: the lines around ours
/// can belong to another server, and its `env` can hold a key.
fn diff(path: &Path, before: &str, after: &str) -> String {
    let name = path.display().to_string();
    let text = similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(0)
        .header(&name, &name)
        .to_string();
    crate::redact::redact_secrets(&text)
}

/// Make one planned change, recording whether it was written.
fn apply(planned: &mut Planned) {
    if !planned.change.status.changes_something() {
        return;
    }
    match write(
        &planned.change.path,
        planned.before.as_deref(),
        planned.after.as_deref(),
    ) {
        Ok(()) => planned.change.written = true,
        Err(error) => planned.change.note = Some(format!("not written: {error}")),
    }
}

/// Write `after` to `path` (or delete it for `None`), but only if the file
/// still holds `before`: an agent may have rewritten it since setup read it.
fn write(path: &Path, before: Option<&str>, after: Option<&str>) -> io::Result<()> {
    if read_optional(path)?.as_deref() != before {
        return Err(io::Error::other(
            "it changed while setup was working; run setup again",
        ));
    }
    let Some(after) = after else {
        fs::remove_file(path)?;
        // The skill's own directory, which setup made; `remove_dir` only
        // removes an empty one, so anything else in it stays.
        if let Some(directory) = path.parent() {
            let _ = fs::remove_dir(directory);
        }
        return Ok(());
    };
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    // Write beside it and rename over it, so a crash halfway leaves the old
    // file, not half a new one. The rename is atomic on one filesystem.
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".relish-setup.tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, after)?;
    if let Ok(meta) = fs::metadata(path) {
        fs::set_permissions(&temporary, meta.permissions())?;
    }
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// The whole answer: [`setup`]'s data, what to run next, and warnings.
///
/// `rerun` is this command line, for the `--yes` step.
pub fn answer(
    options: &SetupOptions,
    home: &Home,
    skill: Option<&str>,
    build: BuildInfo,
    rerun: &str,
) -> Envelope<SetupData> {
    let data = setup(options, home, skill);
    let changes: Vec<(Client, Change)> = data
        .agents
        .iter()
        .flat_map(|plan| {
            plan.changes
                .iter()
                .map(move |change| (plan.agent, change.clone()))
        })
        .collect();
    let mut warnings = Vec::new();
    let mut next = Vec::new();
    if changes.is_empty() {
        let known: Vec<&str> = Client::ALL.iter().map(|client| client.as_str()).collect();
        warnings.push(format!(
            "found none of {}; name one to set it up anyway, e.g. relish agent setup cursor",
            known.join(", ")
        ));
    }
    let pending = changes
        .iter()
        .any(|(_, change)| change.status.changes_something());
    if !data.applied && pending {
        next.push(NextStep {
            run: format!("{} --yes", without_dry_run(rerun)),
            why: "make exactly these changes, once a human has read the diffs".to_string(),
            read_only: false,
        });
    }
    for (client, change) in &changes {
        if change.status == ChangeStatus::Conflict {
            warnings.push(format!(
                "{}: {} already has a {} entry setup didn't write, so setup left it alone",
                client.as_str(),
                clients::tilde(&change.path, home),
                change.entry
            ));
        }
    }
    let mut restart: Vec<&str> = changes
        .iter()
        .filter(|(_, change)| change.written)
        .map(|(client, _)| client.as_str())
        .collect();
    restart.dedup();
    if !restart.is_empty() {
        warnings.push(format!(
            "restart {} to load the change; each still asks before it calls a tool",
            restart.join(", ")
        ));
    }
    let mut envelope = Envelope::answered(CommandName::Setup, build, data);
    for step in next {
        envelope = envelope.with_next(step);
    }
    for warning in warnings {
        envelope = envelope.with_warning(warning);
    }
    envelope
}

fn without_dry_run(rerun: &str) -> String {
    rerun
        .split(' ')
        .filter(|word| *word != "--dry-run" && *word != "--print")
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `relish agent setup` against this user's home.
pub async fn run(options: &SetupOptions, max_bytes: usize, rerun: &str) -> Rendered {
    let options = options.clone();
    let rerun_for_task = rerun.to_string();
    // File I/O, and packing the source digest on a dev build, so off the
    // async worker threads.
    let outcome = tokio::task::spawn_blocking(move || {
        let home = Home::current()?;
        Some(answer(
            &options,
            &home,
            Some(SKILL_MD),
            BuildInfo::current(),
            &rerun_for_task,
        ))
    })
    .await;
    match outcome {
        Ok(Some(envelope)) => budget::render(&envelope, max_bytes, rerun),
        Ok(None) => failed(
            AgentError::new(ErrorCode::Internal, "couldn't find the home directory")
                .with_hint("set HOME"),
            max_bytes,
            rerun,
        ),
        Err(error) => failed(
            AgentError::new(ErrorCode::Internal, format!("setup stopped: {error}")),
            max_bytes,
            rerun,
        ),
    }
}

fn failed(error: AgentError, max_bytes: usize, rerun: &str) -> Rendered {
    let envelope: Envelope<()> = Envelope::failed(CommandName::Setup, BuildInfo::current(), error);
    budget::render(&envelope, max_bytes, rerun)
}

#[cfg(test)]
mod tests;
