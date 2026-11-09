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
//!
//! When the saved context holds the cluster's admin token, setup also makes
//! `relish mcp` its own read-only token (#1730,
//! [`crate::relish::mcp::credentials`]), so the agents it wires never carry
//! admin credentials. The preview says so; `--yes` makes it; `--remove`
//! revokes it.

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
    /// `relish mcp`'s own read-only tokens, made from the context's admin
    /// one so no agent carries admin credentials: `mcp-token.json` to make,
    /// or with `--remove` every token file beside the context
    /// (`mcp-token*.json` and `mcp-propose-token*.json`), each one's token
    /// revoked and the file deleted. Empty
    /// when the context isn't an admin one (or there's none), so there's
    /// nothing to do.
    pub mcp_tokens: Vec<Change>,
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
    /// `relish mcp`'s read-only token, kept beside the context.
    McpToken,
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

/// One of `relish mcp`'s token files.
pub struct TokenFile {
    /// Where it is, or would be.
    pub path: PathBuf,
    /// Whether a token that's still good is there (for `--remove`, whether
    /// the file is there at all).
    pub stored: bool,
    /// The file's JSON with the secret redacted, for the diff.
    pub preview: String,
    /// Why a token that's there doesn't count, for the answer to say.
    pub note: Option<String>,
}

/// What setup can do about `relish mcp`'s read-only tokens, when the saved
/// context holds an admin one.
pub struct TokenPlan {
    /// The files: `mcp-token.json` to make, or every token file there is
    /// to remove.
    pub files: Vec<TokenFile>,
    /// Make the token at a path ([`Action::Install`]) or revoke and delete
    /// it ([`Action::Remove`]). `Ok(false)` means there was nothing to do:
    /// the cluster says the context's token isn't admin, or the file had
    /// already gone. Called only with `--yes`.
    pub apply: ApplyToken,
}

/// Makes or removes the token at a path; see [`TokenPlan::apply`].
pub type ApplyToken = Box<dyn FnMut(Action, &Path) -> Result<bool, String> + Send>;

/// Plan, and with `--yes` make, the change for every token file.
fn plan_tokens(token: TokenPlan, action: Action, yes: bool) -> Vec<Change> {
    let TokenPlan { files, mut apply } = token;
    files
        .into_iter()
        .map(|file| plan_token(file, &mut apply, action, yes))
        .collect()
}

/// Plan, and with `--yes` make, the change for one token file.
fn plan_token(
    file: TokenFile,
    apply: &mut dyn FnMut(Action, &Path) -> Result<bool, String>,
    action: Action,
    yes: bool,
) -> Change {
    let exists = file.stored || (action == Action::Remove && file.path.exists());
    let status = match (action, exists) {
        (Action::Install, false) => ChangeStatus::Add,
        (Action::Remove, true) => ChangeStatus::Remove,
        (Action::Install, true) | (Action::Remove, false) => ChangeStatus::Unchanged,
    };
    let mut change = change(
        ChangeKind::McpToken,
        file.path.clone(),
        "token".to_string(),
        status,
    );
    change.note = file.note;
    if !status.changes_something() {
        return change;
    }
    // The file's JSON, never the secret: the preview has it redacted.
    let contents = format!("{}\n", file.preview);
    change.diff = Some(match action {
        Action::Install => diff(&file.path, "", &contents),
        Action::Remove => diff(&file.path, &contents, ""),
    });
    if yes {
        match apply(action, &file.path) {
            Ok(true) => change.written = true,
            Ok(false) => {
                change.note = Some(match action {
                    Action::Install => "the cluster says the context's token isn't admin, so \
                                        relish mcp uses it as it is"
                        .to_string(),
                    Action::Remove => "it was already gone".to_string(),
                })
            }
            Err(error) => change.note = Some(format!("not written: {error}")),
        }
    }
    change
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
pub fn setup(
    options: &SetupOptions,
    home: &Home,
    skill: Option<&str>,
    token: Option<TokenPlan>,
) -> SetupData {
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
        mcp_tokens: token.map_or_else(Vec::new, |token| plan_tokens(token, action, options.yes)),
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
    token: Option<TokenPlan>,
    build: BuildInfo,
    rerun: &str,
) -> Envelope<SetupData> {
    let data = setup(options, home, skill, token);
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
        .map(|(_, change)| change)
        .chain(&data.mcp_tokens)
        .any(|change| change.status.changes_something());
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
    for note in data
        .mcp_tokens
        .iter()
        .filter_map(|token| token.note.as_deref())
        .filter(|note| note.starts_with("not written"))
    {
        warnings.push(format!(
            "relish mcp's read-only token: {note}; relish mcp will try again when it starts, \
             and sends only a read-only token, never the admin one"
        ));
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
    let action = if options.remove {
        Action::Remove
    } else {
        Action::Install
    };
    let token = token_plan(action).await;
    // File I/O, and packing the source digest on a dev build, so off the
    // async worker threads.
    let outcome = tokio::task::spawn_blocking(move || {
        let home = Home::current()?;
        Some(answer(
            &options,
            &home,
            Some(SKILL_MD),
            token,
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

/// The token plan for this machine: `None` unless the token `relish mcp`
/// would send is the saved context's admin one.
///
/// To make the token, a stored one that's still good on disk is checked
/// with the cluster too (one cheap admin-only read), so a token revoked or
/// rotated behind `relish mcp`'s back is replaced, not called unchanged.
/// To remove, every token file beside the context is in the plan.
async fn token_plan(action: Action) -> Option<TokenPlan> {
    use crate::relish::client::{TokenOrigin, token_origin};
    use crate::relish::mcp::credentials;
    if token_origin() != TokenOrigin::ContextAdmin {
        return None;
    }
    let context_path = crate::relish::local_context::default_path().ok()?;
    let context = crate::relish::local_context::LocalContext::load(&context_path)
        .ok()
        .flatten()?;
    let admin = context.client(None, None).ok()?;
    let scope = credentials::Scope {
        owner: context.owner.clone(),
        endpoint: context.endpoint.clone(),
        namespace: None,
        purpose: credentials::Purpose::Read,
    };
    let now = credentials::now();
    let mut rejected = None;
    let files = match action {
        Action::Install => {
            let path = credentials::path_beside(&context_path, None).ok()?;
            let usable = credentials::StoredToken::load(&path)
                .ok()
                .flatten()
                .filter(|stored| stored.usable(&scope, now));
            let mut note = None;
            if let Some(stored) = usable.as_ref()
                && credentials::accepted(&admin, &stored.token).await == Some(false)
            {
                note = Some(
                    "the cluster rejects the stored token (revoked or rotated); a new one \
                     replaces it"
                        .to_string(),
                );
                rejected = Some(stored.token.clone());
            }
            vec![TokenFile {
                path,
                stored: usable.is_some() && rejected.is_none(),
                preview: credentials::shape(&scope, None, now),
                note,
            }]
        }
        Action::Remove => {
            let mut files: Vec<TokenFile> = credentials::files_beside(&context_path)
                .ok()?
                .into_iter()
                .map(|path| {
                    let stored = credentials::StoredToken::load(&path).ok().flatten();
                    TokenFile {
                        preview: credentials::shape(&scope, stored.as_ref(), now),
                        path,
                        stored: true,
                        note: None,
                    }
                })
                .collect();
            if files.is_empty() {
                files.push(TokenFile {
                    path: credentials::path_beside(&context_path, None).ok()?,
                    stored: false,
                    preview: String::new(),
                    note: None,
                });
            }
            files
        }
    };
    let runtime = tokio::runtime::Handle::current();
    let apply = move |action: Action, path: &Path| -> Result<bool, String> {
        // `apply` runs inside `spawn_blocking`, where blocking on the
        // runtime is allowed: that thread isn't one of its workers.
        runtime
            .block_on(async {
                match action {
                    Action::Install => credentials::obtain(
                        path,
                        &scope,
                        &admin,
                        credentials::now(),
                        rejected.as_deref(),
                    )
                    .await
                    .map(|(_, obtained)| obtained != credentials::Obtained::NotNeeded),
                    Action::Remove => credentials::remove(path, &admin).await,
                }
            })
            .map_err(|error| error.to_string())
    };
    Some(TokenPlan {
        files,
        apply: Box::new(apply),
    })
}

fn failed(error: AgentError, max_bytes: usize, rerun: &str) -> Rendered {
    let envelope: Envelope<()> = Envelope::failed(CommandName::Setup, BuildInfo::current(), error);
    budget::render(&envelope, max_bytes, rerun)
}

#[cfg(test)]
mod tests;
