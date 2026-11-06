//! `relish agent guide`, and every agent-facing file made from the same
//! chapter.
//!
//! The manual chapter "Working with an AI agent" is the one source. `relish
//! agent guide` prints it from the copy embedded in the binary, and
//! [`generate`] turns it into the files an agent finds elsewhere: `llms.txt`
//! and the `/agents` page on the website, the `reliaburger` skill, the Claude
//! Code plugin and its marketplace, and the generated block of the
//! repository's `AGENTS.md`. A test in `tests/suite/agent_docs.rs` compares
//! each file on disk with what [`generate`] makes, so none can drift, and
//! `make agent-docs` rewrites them.
//!
//! The chapter marks the parts the shorter files reuse with HTML comments,
//! such as `<!-- agent-brief:start -->` and `<!-- agent-brief:end -->`: see
//! [`Region`]. The command table between the `agent-commands` markers is
//! written from the clap definition of `relish agent`, so a new command shows
//! up in every file the next time someone runs `make agent-docs`, and the
//! drift test fails until they do.

use std::fmt::Write as _;

use clap::{Args, Command};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;

use super::{AgentError, AgentOptions, BuildInfo, CommandName, Envelope, ErrorCode, NextStep};
use super::{Rendered, render};

/// The chapter's file name inside `docs/manual/`.
pub const CHAPTER_FILE: &str = "16_working-with-an-ai-agent.md";

/// The command that rewrites every generated file, quoted in each failure.
pub const REGENERATE_COMMAND: &str = "make agent-docs";

const REPOSITORY: &str = "https://github.com/reliaburger/reliaburger";
const SITE: &str = "https://reliaburger.com";
const AUTHOR: &str = "Miko Pawlikowski";
const LICENSE: &str = "Apache-2.0";

/// The skill's name, which the Agent Skills format says must match its
/// directory.
pub const SKILL_NAME: &str = "reliaburger";

/// The longest skill description the Agent Skills format allows.
pub const SKILL_DESCRIPTION_MAX: usize = 1024;

/// When a client should load the skill. It leads the skill's description,
/// because a client decides from the description alone.
const SKILL_TRIGGER: &str = "Use when operating, debugging or upgrading a Reliaburger cluster, \
or when you suspect a bug in Reliaburger itself.";

/// Paths of the generated files, relative to the repository root.
pub mod paths {
    /// The chapter itself: its command table is generated.
    pub const CHAPTER: &str = "docs/manual/16_working-with-an-ai-agent.md";
    /// The repository's instructions for coding agents.
    pub const AGENTS_MD: &str = "AGENTS.md";
    /// The [llms.txt](https://llmstxt.org/) index of the manual.
    pub const LLMS_TXT: &str = "docs/website/llms.txt";
    /// The website's `/agents.md`.
    pub const AGENTS_PAGE_MARKDOWN: &str = "docs/website/agents.md";
    /// The website's `/agents`.
    pub const AGENTS_PAGE_HTML: &str = "docs/website/agents.html";
    /// The Agent Skills file.
    pub const SKILL: &str = "plugins/reliaburger/skills/reliaburger/SKILL.md";
    /// The Claude Code plugin's manifest.
    pub const PLUGIN: &str = "plugins/reliaburger/.claude-plugin/plugin.json";
    /// The MCP server the Claude Code plugin starts: `relish mcp`.
    pub const PLUGIN_MCP: &str = "plugins/reliaburger/.mcp.json";
    /// The Claude Code marketplace this repository is.
    pub const MARKETPLACE: &str = ".claude-plugin/marketplace.json";
}

/// A marked part of the chapter (or of `AGENTS.md`) that a generated file
/// reuses or that [`generate`] rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// One paragraph saying what Reliaburger and `relish agent` are: the
    /// `llms.txt` blockquote, the skill's and the plugin's description.
    Summary,
    /// The rules an operating agent follows, short enough for a skill.
    Brief,
    /// The command table, written from the CLI definition.
    Commands,
    /// The repository's rules for agents that send a bug or a fix.
    Rules,
    /// The block of `AGENTS.md` this module writes.
    AgentsFile,
}

impl Region {
    fn name(self) -> &'static str {
        match self {
            Region::Summary => "summary",
            Region::Brief => "brief",
            Region::Commands => "commands",
            Region::Rules => "rules",
            Region::AgentsFile => "generated",
        }
    }

    /// The comment that opens the region.
    pub fn start(self) -> String {
        format!("<!-- agent-{}:start -->", self.name())
    }

    /// The comment that closes the region.
    pub fn end(self) -> String {
        format!("<!-- agent-{}:end -->", self.name())
    }
}

/// Why the files couldn't be generated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuideError {
    /// A region's markers are missing, repeated or out of order.
    #[error("{file} needs exactly one {start} followed by one {end}")]
    Markers {
        file: String,
        start: String,
        end: String,
    },
    /// The chapter's summary makes the skill's description too long.
    #[error("the skill description is {length} characters; the limit is {limit}", limit = SKILL_DESCRIPTION_MAX)]
    SkillDescriptionTooLong { length: usize },
}

/// Byte offsets of a region's body: just after its start marker's line and
/// at its end marker.
fn region_bounds(file: &str, text: &str, region: Region) -> Result<(usize, usize), GuideError> {
    let (start, end) = (region.start(), region.end());
    let error = || GuideError::Markers {
        file: file.to_string(),
        start: region.start(),
        end: region.end(),
    };
    if text.matches(&start).count() != 1 || text.matches(&end).count() != 1 {
        return Err(error());
    }
    let open = text.find(&start).ok_or_else(error)? + start.len();
    let close = text.find(&end).ok_or_else(error)?;
    if close < open {
        return Err(error());
    }
    Ok((open, close))
}

/// The text between a region's markers, without the blank lines around it.
pub fn region<'a>(file: &str, text: &'a str, region: Region) -> Result<&'a str, GuideError> {
    let (open, close) = region_bounds(file, text, region)?;
    Ok(text[open..close].trim_matches('\n'))
}

/// `text` with the body of `region` replaced by `body`.
pub fn replace_region(
    file: &str,
    text: &str,
    region: Region,
    body: &str,
) -> Result<String, GuideError> {
    let (open, close) = region_bounds(file, text, region)?;
    let body = body.trim_matches('\n');
    let mut out = String::with_capacity(text.len() + body.len());
    out.push_str(&text[..open]);
    out.push('\n');
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
    out.push_str(&text[close..]);
    Ok(out)
}

/// `relish agent` as clap defines it, without the rest of `relish`.
pub fn agent_command() -> Command {
    AgentOptions::augment_args(Command::new("agent"))
}

/// One `relish agent` command and what it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CommandSummary {
    /// The command as typed, e.g. `relish agent source grep`.
    pub command: String,
    /// Its one-line help.
    pub about: String,
}

/// Every visible `relish agent` command that runs something, in the order
/// clap defines them: `source` itself is left out and its five commands are
/// listed.
pub fn commands(agent: &Command) -> Vec<CommandSummary> {
    let mut out = Vec::new();
    collect_commands(agent, "relish agent", &mut out);
    out
}

fn collect_commands(command: &Command, prefix: &str, out: &mut Vec<CommandSummary>) {
    for sub in command.get_subcommands() {
        if sub.is_hide_set() || sub.get_name() == "help" {
            continue;
        }
        let name = format!("{prefix} {}", sub.get_name());
        let has_children = sub
            .get_subcommands()
            .any(|child| !child.is_hide_set() && child.get_name() != "help");
        if has_children {
            collect_commands(sub, &name, out);
            continue;
        }
        let about = sub
            .get_about()
            .map(|about| about.to_string())
            .unwrap_or_default();
        out.push(CommandSummary {
            command: name,
            about: about.split_whitespace().collect::<Vec<_>>().join(" "),
        });
    }
}

/// The commands as a Markdown table.
pub fn command_table(commands: &[CommandSummary]) -> String {
    let mut table = String::from("| Command | What it does |\n|---|---|\n");
    for command in commands {
        let _ = writeln!(
            table,
            "| `{}` | {} |",
            command.command,
            command.about.replace('|', "\\|")
        );
    }
    table
}

/// Whether `line` is one of the comments that steer generation or the doc
/// snippet checks, which no reader needs to see.
fn is_marker(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("<!--")
        && line.ends_with("-->")
        && (line.contains("agent-") || line.contains("snippet:"))
}

/// `markdown` without its marker comments.
fn without_markers(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut blank = false;
    for line in markdown.lines().filter(|line| !is_marker(line)) {
        // Dropping a marker can leave two blank lines; keep one.
        let is_blank = line.trim().is_empty();
        if is_blank && blank {
            continue;
        }
        blank = is_blank;
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A region's text joined into one line, for a blockquote or a description.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One `## ` section of the chapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GuideSection {
    /// The heading, without `## `. The first section has the chapter's
    /// title.
    pub heading: String,
    /// Everything under the heading, as Markdown.
    pub markdown: String,
}

/// `data` for `relish agent guide`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GuideData {
    /// The manual chapter this is, e.g. `16_working-with-an-ai-agent.md`.
    pub chapter: String,
    /// The chapter's title.
    pub title: String,
    /// The chapter, one section at a time and in order. A trimmed answer
    /// drops sections from the end.
    pub sections: Vec<GuideSection>,
    /// Every `relish agent` command this binary has, from its own CLI
    /// definition.
    pub commands: Vec<CommandSummary>,
}

/// The chapter split at its `## ` headings, outside code fences, with the
/// marker comments taken out.
pub fn sections(chapter: &str) -> (String, Vec<GuideSection>) {
    let cleaned = without_markers(chapter);
    let mut title = String::new();
    let mut sections: Vec<GuideSection> = Vec::new();
    let mut heading = String::new();
    let mut body = String::new();
    let mut in_fence = false;
    for line in cleaned.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && title.is_empty() && line.starts_with("# ") {
            title = line[2..].trim().to_string();
            heading = title.clone();
            continue;
        }
        if !in_fence && let Some(next) = line.strip_prefix("## ") {
            sections.push(GuideSection {
                heading: std::mem::replace(&mut heading, next.trim().to_string()),
                markdown: std::mem::take(&mut body).trim().to_string(),
            });
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    sections.push(GuideSection {
        heading,
        markdown: body.trim().to_string(),
    });
    (title, sections)
}

/// `data` for `relish agent guide` from a chapter and the CLI definition.
pub fn guide_data(chapter: &str, agent: &Command) -> GuideData {
    let (title, sections) = sections(chapter);
    GuideData {
        chapter: CHAPTER_FILE.to_string(),
        title,
        sections,
        commands: commands(agent),
    }
}

/// The envelope `relish agent guide` prints for `chapter`.
pub fn guide_envelope(chapter: &str, agent: &Command, relish: BuildInfo) -> Envelope<GuideData> {
    Envelope::answered(CommandName::Guide, relish, guide_data(chapter, agent)).with_next(NextStep {
        run: "relish agent source version".to_string(),
        why: "which source this relish carries, and whether the nodes run it".to_string(),
        read_only: true,
    })
}

/// Run `relish agent guide` against the chapter embedded in this binary.
pub fn run(max_bytes: usize, rerun: &str) -> Rendered {
    let Some(asset) = crate::relish::manual::assets::ManualAssets::get(CHAPTER_FILE) else {
        let error = AgentError::new(
            ErrorCode::Internal,
            format!("this relish was built without the manual chapter {CHAPTER_FILE}"),
        );
        return super::failure(CommandName::Guide, error, max_bytes, rerun);
    };
    let chapter = String::from_utf8_lossy(asset.data.as_ref());
    render(
        &guide_envelope(&chapter, &agent_command(), BuildInfo::current()),
        max_bytes,
        rerun,
    )
}

// ---------------------------------------------------------------------------
// The generated files
// ---------------------------------------------------------------------------

/// A manual chapter, as `llms.txt` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualEntry {
    /// File name inside `docs/manual/`.
    pub file: String,
    /// The chapter's `# ` title.
    pub title: String,
}

/// What [`generate`] reads.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The chapter as it is on disk.
    pub chapter: &'a str,
    /// `AGENTS.md` as it is on disk; only its generated block changes.
    pub agents_md: &'a str,
    /// Every manual chapter, in order.
    pub manual: &'a [ManualEntry],
    /// The Cargo package version, which pins `llms.txt`'s links to the
    /// release tag and versions the plugin.
    pub version: &'a str,
}

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artefact {
    /// Path relative to the repository root.
    pub path: &'static str,
    /// What the file should hold.
    pub contents: String,
}

/// The first line of every generated Markdown file that can carry one.
fn generated_note() -> String {
    format!(
        "<!-- Generated from {} by `{REGENERATE_COMMAND}`: edit the chapter, not this file. -->",
        paths::CHAPTER
    )
}

/// Every generated file, from the chapter and the CLI definition.
///
/// The chapter comes first, with its command table rewritten, and every
/// other file is made from that fresh copy.
pub fn generate(inputs: &Inputs<'_>, agent: &Command) -> Result<Vec<Artefact>, GuideError> {
    let table = command_table(&commands(agent));
    let chapter = replace_region(paths::CHAPTER, inputs.chapter, Region::Commands, &table)?;
    let summary = one_line(region(paths::CHAPTER, &chapter, Region::Summary)?);
    let brief = region(paths::CHAPTER, &chapter, Region::Brief)?.to_string();
    let rules = region(paths::CHAPTER, &chapter, Region::Rules)?.to_string();
    let (title, _) = sections(&chapter);
    let page = without_markers(&chapter);

    let agents_md = replace_region(
        paths::AGENTS_MD,
        inputs.agents_md,
        Region::AgentsFile,
        &agents_md_block(&rules, &brief),
    )?;

    Ok(vec![
        Artefact {
            path: paths::CHAPTER,
            contents: chapter.clone(),
        },
        Artefact {
            path: paths::AGENTS_MD,
            contents: agents_md,
        },
        Artefact {
            path: paths::LLMS_TXT,
            contents: llms_txt(&summary, &brief, &title, inputs.manual, inputs.version),
        },
        Artefact {
            path: paths::AGENTS_PAGE_MARKDOWN,
            contents: format!("{}\n\n{page}", generated_note()),
        },
        Artefact {
            path: paths::AGENTS_PAGE_HTML,
            contents: agents_html(&title, &summary, &page),
        },
        Artefact {
            path: paths::SKILL,
            contents: skill_md(&summary, &brief, &table)?,
        },
        Artefact {
            path: paths::PLUGIN,
            contents: plugin_json(&summary, inputs.version),
        },
        Artefact {
            path: paths::PLUGIN_MCP,
            contents: plugin_mcp_json(),
        },
        Artefact {
            path: paths::MARKETPLACE,
            contents: marketplace_json(&summary),
        },
    ])
}

/// The paths among `artefacts` whose file on disk differs, or is missing.
/// `on_disk` reads a repository-relative path.
pub fn stale_paths(
    artefacts: &[Artefact],
    on_disk: impl Fn(&str) -> Option<String>,
) -> Vec<&'static str> {
    artefacts
        .iter()
        .filter(|artefact| on_disk(artefact.path).as_deref() != Some(artefact.contents.as_str()))
        .map(|artefact| artefact.path)
        .collect()
}

fn agents_md_block(rules: &str, brief: &str) -> String {
    format!(
        "{note}\n\n{rules}\n\n## Debugging a running cluster\n\n\
         If you're operating or debugging a Reliaburger cluster rather than editing \
         this code, the manual's [Working with an AI agent]({chapter}) chapter is for \
         you, and `relish agent guide` prints it.\n\n{brief}",
        note = generated_note(),
        chapter = paths::CHAPTER,
    )
}

fn llms_txt(
    summary: &str,
    brief: &str,
    title: &str,
    manual: &[ManualEntry],
    version: &str,
) -> String {
    let raw = format!("https://raw.githubusercontent.com/reliaburger/reliaburger/v{version}");
    let tree = format!("{REPOSITORY}/tree/v{version}");
    let mut out = format!("# Reliaburger\n\n> {summary}\n\n{brief}\n\n## Start here\n\n");
    let _ = writeln!(
        out,
        "- [{title}]({raw}/{chapter}): the agent chapter; `relish agent guide` prints \
         the copy that matches your installed `relish`",
        chapter = paths::CHAPTER,
    );
    out.push_str("\n## Manual\n\n");
    for entry in manual.iter().filter(|entry| entry.file != CHAPTER_FILE) {
        let _ = writeln!(out, "- [{}]({raw}/docs/manual/{})", entry.title, entry.file);
    }
    out.push_str("\n## Optional\n\n");
    let _ = writeln!(
        out,
        "- [Building Reliaburger]({tree}/docs/book): the book, how and why each part was built"
    );
    let _ = writeln!(
        out,
        "- [Design documents]({tree}/docs/design): one per subsystem"
    );
    let _ = writeln!(
        out,
        "- [Whitepaper]({raw}/docs/whitepaper.md): the architecture in one document"
    );
    out
}

/// `text` with `&`, `<`, `>` and `"` escaped for HTML.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn agents_html(title: &str, summary: &str, page: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut body = String::new();
    html::push_html(&mut body, Parser::new_ext(page, Options::ENABLE_TABLES));
    format!(
        r##"<!doctype html>
<!-- Generated from {chapter} by `{REGENERATE_COMMAND}`: edit the chapter, not this file. -->
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="description" content="{description}">
  <meta name="theme-color" content="#181b19">
  <title>{title} · Reliaburger</title>
  <link rel="stylesheet" href="style.css">
  <link rel="alternate" type="text/markdown" href="agents.md">
</head>
<body>
  <a class="skip" href="#main">Skip to content</a>
  <div class="page">
    <header class="masthead">
      <a class="wordmark" href="./" aria-label="Reliaburger home"><span class="mark" aria-hidden="true">r/</span>reliaburger</a>
      <nav aria-label="Main navigation"><a href="./#install">Install</a><a href="./agents.md">Markdown</a><a href="./llms.txt">llms.txt</a><a href="{REPOSITORY}">GitHub ↗</a></nav>
    </header>
    <main id="main" class="agents-page">
{body}    </main>
    <footer><span>Reliaburger · by {AUTHOR}</span><a href="./agents.md">This page as Markdown</a><span class="footer-note">Generated from the manual chapter {title_escaped}. Static HTML and CSS. The existing copy script adds command buttons. No tracking or third-party requests.</span></footer>
  </div>
  <script src="assets/copy.js" defer></script>
</body>
</html>
"##,
        chapter = paths::CHAPTER,
        description = escape_html(summary),
        title = escape_html(title),
        title_escaped = escape_html(&format!("“{title}”")),
    )
}

fn skill_description(summary: &str) -> Result<String, GuideError> {
    let description = format!("{SKILL_TRIGGER} {summary}");
    let length = description.chars().count();
    if length > SKILL_DESCRIPTION_MAX {
        return Err(GuideError::SkillDescriptionTooLong { length });
    }
    Ok(description)
}

fn skill_md(summary: &str, brief: &str, table: &str) -> Result<String, GuideError> {
    // A JSON string is a valid YAML double-quoted scalar, which survives the
    // colons and backticks in the summary.
    let description = serde_json::Value::String(skill_description(summary)?).to_string();
    Ok(format!(
        "---\nname: {SKILL_NAME}\ndescription: {description}\nlicense: {LICENSE}\n---\n\n\
         {note}\n\n# Reliaburger\n\n{brief}\n\n## Commands\n\n{table}\n\
         `relish agent guide` prints the whole chapter this skill is made from, as the \
         installed `relish` has it, and `relish agent schema` the shape of every answer.\n",
        note = generated_note(),
    ))
}

fn pretty_json(value: &serde_json::Value) -> String {
    // Serialising a `Value` built from `json!` can't fail.
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

fn plugin_json(summary: &str, version: &str) -> String {
    pretty_json(&json!({
        "name": SKILL_NAME,
        "version": version,
        "description": summary,
        "author": {"name": AUTHOR},
        "homepage": format!("{SITE}/agents"),
        "repository": REPOSITORY,
        "license": LICENSE,
        "keywords": ["reliaburger", "relish", "orchestrator", "containers", "sre"],
    }))
}

/// The plugin's `.mcp.json`: the same server, under the same name, that
/// `relish agent setup` writes into an agent's own config.
fn plugin_mcp_json() -> String {
    let mut words = super::setup::MCP_COMMAND_LINE.iter();
    let command = words.next().copied().unwrap_or("relish");
    let args: Vec<&str> = words.copied().collect();
    let mut servers = serde_json::Map::new();
    servers.insert(
        super::setup::MCP_SERVER_NAME.to_string(),
        json!({"command": command, "args": args}),
    );
    pretty_json(&json!({ "mcpServers": servers }))
}

fn marketplace_json(summary: &str) -> String {
    pretty_json(&json!({
        "name": "reliaburger",
        "description": "Reliaburger's own Claude Code plugin, from its repository.",
        "owner": {"name": AUTHOR, "url": REPOSITORY},
        "plugins": [{
            "name": SKILL_NAME,
            "source": "./plugins/reliaburger",
            "description": summary,
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_CHAPTER: &str = "# Working with an AI agent\n\n\
<!-- agent-summary:start -->\nReliaburger is an orchestrator.\nIts CLI is `relish`.\n<!-- agent-summary:end -->\n\n\
Intro paragraph.\n\n## In brief\n\n\
<!-- agent-brief:start -->\n- **Run `relish agent guide` first.**\n<!-- agent-brief:end -->\n\n\
## Commands\n\n<!-- agent-commands:start -->\nstale table\n<!-- agent-commands:end -->\n\n\
```sh\n## not a heading inside a fence\nrelish agent guide\n```\n\n\
## Sending us a fix\n\n<!-- agent-rules:start -->\n- **Disclose.**\n<!-- agent-rules:end -->\n";

    const FIXTURE_AGENTS_MD: &str = "# Agents working on Reliaburger\n\nHand-written intro.\n\n\
<!-- agent-generated:start -->\nold\n<!-- agent-generated:end -->\n";

    fn fixture_manual() -> Vec<ManualEntry> {
        vec![
            ManualEntry {
                file: "00_getting-started.md".to_string(),
                title: "Getting started".to_string(),
            },
            ManualEntry {
                file: CHAPTER_FILE.to_string(),
                title: "Working with an AI agent".to_string(),
            },
        ]
    }

    /// A stand-in for `relish agent`, so the snapshots don't move every time
    /// a real command lands.
    fn fixture_agent() -> Command {
        Command::new("agent")
            .subcommand(Command::new("guide").about("This chapter, as Markdown sections."))
            .subcommand(Command::new("hidden").hide(true))
            .subcommand(
                Command::new("source")
                    .about("The source.")
                    .subcommand(Command::new("grep").about("Search it | quickly.")),
            )
    }

    fn generated() -> Vec<Artefact> {
        let manual = fixture_manual();
        let inputs = Inputs {
            chapter: FIXTURE_CHAPTER,
            agents_md: FIXTURE_AGENTS_MD,
            manual: &manual,
            version: "0.2.7",
        };
        generate(&inputs, &fixture_agent()).unwrap()
    }

    fn artefact(path: &str) -> String {
        generated()
            .into_iter()
            .find(|artefact| artefact.path == path)
            .unwrap()
            .contents
    }

    #[test]
    fn commands_come_from_the_cli_definition_leaves_only_without_hidden_ones() {
        let names: Vec<String> = commands(&fixture_agent())
            .into_iter()
            .map(|command| command.command)
            .collect();
        assert_eq!(names, ["relish agent guide", "relish agent source grep"]);
    }

    #[test]
    fn the_real_cli_lists_guide_and_every_source_command() {
        let names: Vec<String> = commands(&agent_command())
            .into_iter()
            .map(|command| command.command)
            .collect();
        for expected in [
            "relish agent guide",
            "relish agent version",
            "relish agent schema",
            "relish agent source version",
            "relish agent source grep",
            "relish agent source extract",
        ] {
            assert!(names.iter().any(|name| name == expected), "{names:?}");
        }
        assert!(!names.iter().any(|name| name == "relish agent source"));
    }

    #[test]
    fn a_missing_or_repeated_marker_is_an_error() {
        assert!(region("x.md", "no markers", Region::Brief).is_err());
        let twice =
            "<!-- agent-brief:start -->\n<!-- agent-brief:start -->\n<!-- agent-brief:end -->";
        assert!(region("x.md", twice, Region::Brief).is_err());
        let backwards = "<!-- agent-brief:end -->\n<!-- agent-brief:start -->";
        assert!(region("x.md", backwards, Region::Brief).is_err());
    }

    #[test]
    fn replacing_a_region_keeps_everything_around_it() {
        let text = "before\n<!-- agent-brief:start -->\nold\n<!-- agent-brief:end -->\nafter\n";
        let replaced = replace_region("x.md", text, Region::Brief, "new\n").unwrap();
        assert_eq!(
            replaced,
            "before\n<!-- agent-brief:start -->\nnew\n<!-- agent-brief:end -->\nafter\n"
        );
        assert_eq!(
            replace_region("x.md", &replaced, Region::Brief, "new").unwrap(),
            replaced,
            "replacing is idempotent"
        );
    }

    #[test]
    fn generation_is_deterministic_and_idempotent() {
        let first = generated();
        assert_eq!(first, generated());
        let chapter = &first[0].contents;
        let agents = &first[1].contents;
        let manual = fixture_manual();
        let again = generate(
            &Inputs {
                chapter,
                agents_md: agents,
                manual: &manual,
                version: "0.2.7",
            },
            &fixture_agent(),
        )
        .unwrap();
        assert_eq!(first, again, "regenerating from the output changes nothing");
    }

    #[test]
    fn every_artefact_carries_the_brief_or_the_whole_chapter() {
        let brief = "Run `relish agent guide` first.";
        for artefact in generated() {
            if artefact.path == paths::PLUGIN_MCP {
                // A server entry, with no prose; its own test checks it.
                continue;
            }
            if artefact.path.ends_with(".json") {
                assert!(
                    artefact
                        .contents
                        .contains("Reliaburger is an orchestrator. Its CLI is `relish`."),
                    "{} lacks the summary",
                    artefact.path
                );
            } else if artefact.path.ends_with(".html") {
                assert!(
                    artefact
                        .contents
                        .contains("Run <code>relish agent guide</code> first."),
                    "{} lacks the brief",
                    artefact.path
                );
            } else {
                assert!(
                    artefact.contents.contains(brief),
                    "{} lacks the brief",
                    artefact.path
                );
            }
        }
    }

    #[test]
    fn llms_txt_lists_every_manual_chapter_pinned_to_the_release_tag() {
        let llms = artefact(paths::LLMS_TXT);
        assert!(llms.starts_with("# Reliaburger\n\n> Reliaburger is an orchestrator."));
        assert!(llms.contains(
            "https://raw.githubusercontent.com/reliaburger/reliaburger/v0.2.7/docs/manual/00_getting-started.md"
        ));
        assert_eq!(
            llms.matches(CHAPTER_FILE).count(),
            1,
            "the agent chapter is under Start here only"
        );
    }

    #[test]
    fn a_chapter_changed_without_regenerating_is_stale() {
        let fresh = generated();
        let on_disk = |path: &str| {
            fresh
                .iter()
                .find(|artefact| artefact.path == path)
                .map(|artefact| artefact.contents.clone())
        };
        assert!(stale_paths(&fresh, on_disk).is_empty());

        let manual = fixture_manual();
        let edited = FIXTURE_CHAPTER.replace("first.**", "first, always.**");
        let changed = generate(
            &Inputs {
                chapter: &edited,
                agents_md: FIXTURE_AGENTS_MD,
                manual: &manual,
                version: "0.2.7",
            },
            &fixture_agent(),
        )
        .unwrap();
        let stale = stale_paths(&changed, on_disk);
        for path in [
            paths::CHAPTER,
            paths::AGENTS_MD,
            paths::LLMS_TXT,
            paths::AGENTS_PAGE_MARKDOWN,
            paths::AGENTS_PAGE_HTML,
            paths::SKILL,
        ] {
            assert!(stale.contains(&path), "{path} isn't stale: {stale:?}");
        }
    }

    #[test]
    fn a_skill_description_over_the_limit_is_refused() {
        let long = "word ".repeat(250);
        assert!(matches!(
            skill_description(&long),
            Err(GuideError::SkillDescriptionTooLong { .. })
        ));
    }

    #[test]
    fn sections_split_at_headings_outside_fences_without_markers() {
        let (title, sections) = sections(FIXTURE_CHAPTER);
        assert_eq!(title, "Working with an AI agent");
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(
            headings,
            [
                "Working with an AI agent",
                "In brief",
                "Commands",
                "Sending us a fix"
            ]
        );
        assert!(sections[2].markdown.contains("## not a heading"));
        assert!(sections.iter().all(|s| !s.markdown.contains("<!--")));
    }

    #[test]
    fn the_embedded_chapter_has_every_region() {
        let asset = crate::relish::manual::assets::ManualAssets::get(CHAPTER_FILE).unwrap();
        let chapter = String::from_utf8_lossy(asset.data.as_ref()).into_owned();
        for marked in [
            Region::Summary,
            Region::Brief,
            Region::Commands,
            Region::Rules,
        ] {
            assert!(region(CHAPTER_FILE, &chapter, marked).is_ok(), "{marked:?}");
        }
    }

    /// The headings `relish agent guide` answers with at `max_bytes`, and
    /// whether it said it trimmed any.
    fn guide_headings(max_bytes: usize) -> (Vec<String>, serde_json::Value) {
        let rendered = run(max_bytes, "relish agent guide");
        assert_eq!(rendered.exit, super::super::ExitStatus::Answered);
        let value: serde_json::Value = serde_json::from_str(&rendered.json).unwrap();
        assert_eq!(value["command"], "guide");
        assert_eq!(value["data"]["title"], "Working with an AI agent");
        let headings = value["data"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|section| section["heading"].as_str().unwrap().to_string())
            .collect();
        (headings, value)
    }

    /// `relish agent guide` is the one document an agent reads whole, so
    /// its default budget (64 KiB, not 24) returns every section in one
    /// call, the setup playbook (#1579) last, with no `next` asking for
    /// more.
    #[test]
    fn guide_answers_the_whole_chapter_within_its_default_budget() {
        const PLAYBOOK: &str = "Setting up a cluster for your human";
        let (headings, value) = guide_headings(super::super::GUIDE_MAX_BYTES);
        assert_eq!(value["truncated"], false, "{headings:?}");
        assert_eq!(headings.last().map(String::as_str), Some(PLAYBOOK));
        assert!(
            value["data"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["command"] == "relish agent guide")
        );
        let (whole, _) = guide_headings(128 * 1024);
        assert_eq!(headings, whole);
        for next in value["next"].as_array().unwrap() {
            let run = next["run"].as_str().unwrap();
            assert!(!run.contains("--max-bytes"), "asks for more: {run}");
        }
    }

    /// The guide's default budget exists so the whole chapter comes back in
    /// one call. If chapter 16 outgrows it, someone has to choose: split
    /// the chapter, or raise `GUIDE_MAX_BYTES` knowing what it costs.
    #[test]
    fn the_agent_chapter_fits_the_guides_default_budget() {
        let rendered = run(1024 * 1024, "relish agent guide");
        let whole = rendered.json.len();
        assert!(
            whole <= super::super::GUIDE_MAX_BYTES,
            "relish agent guide is {whole} bytes, over its {} byte default: split \
             {CHAPTER_FILE} or raise GUIDE_MAX_BYTES deliberately",
            super::super::GUIDE_MAX_BYTES
        );
    }

    /// An explicit `--max-bytes` still wins. At the 24 KiB every other
    /// command defaults to, the chapter doesn't fit, so the budget keeps the
    /// front (the brief, the contract, the command table), drops sections
    /// only from the end, and says how to get the rest.
    #[test]
    fn an_explicit_smaller_budget_drops_trailing_sections() {
        let (headings, value) = guide_headings(super::super::DEFAULT_MAX_BYTES);
        let (whole, _) = guide_headings(super::super::GUIDE_MAX_BYTES);
        assert_eq!(value["truncated"], true);
        assert!(
            headings.len() < whole.len() && headings[..] == whole[..headings.len()],
            "only sections from the end go: {headings:?}"
        );
        for kept in ["In brief", "The contract", "Commands"] {
            assert!(headings.iter().any(|h| h == kept), "{kept}: {headings:?}");
        }
        let rerun = value["next"][0]["run"].as_str().unwrap();
        assert!(
            rerun.starts_with("relish agent guide --max-bytes "),
            "{rerun}"
        );
    }

    #[test]
    fn snapshot_of_the_guide_envelope() {
        let build = BuildInfo {
            version: "0.2.7".to_string(),
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
            dirty: None,
            source_digest: None,
        };
        let rendered = render(
            &guide_envelope(FIXTURE_CHAPTER, &fixture_agent(), build),
            super::super::DEFAULT_MAX_BYTES,
            "relish agent guide",
        );
        let value: serde_json::Value = serde_json::from_str(&rendered.json).unwrap();
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn snapshot_of_the_generated_chapter() {
        insta::assert_snapshot!(artefact(paths::CHAPTER));
    }

    #[test]
    fn snapshot_of_agents_md() {
        insta::assert_snapshot!(artefact(paths::AGENTS_MD));
    }

    #[test]
    fn snapshot_of_llms_txt() {
        insta::assert_snapshot!(artefact(paths::LLMS_TXT));
    }

    #[test]
    fn snapshot_of_the_agents_page_markdown() {
        insta::assert_snapshot!(artefact(paths::AGENTS_PAGE_MARKDOWN));
    }

    #[test]
    fn snapshot_of_the_agents_page_html() {
        insta::assert_snapshot!(artefact(paths::AGENTS_PAGE_HTML));
    }

    #[test]
    fn snapshot_of_the_skill() {
        insta::assert_snapshot!(artefact(paths::SKILL));
    }

    #[test]
    fn snapshot_of_the_plugin_manifest() {
        insta::assert_snapshot!(artefact(paths::PLUGIN));
    }

    #[test]
    fn the_plugin_starts_the_server_setup_registers() {
        let value: serde_json::Value = serde_json::from_str(&artefact(paths::PLUGIN_MCP)).unwrap();
        let server = &value["mcpServers"][super::super::setup::MCP_SERVER_NAME];
        let mut line = vec![server["command"].as_str().unwrap()];
        line.extend(
            server["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap()),
        );
        assert_eq!(line, super::super::setup::MCP_COMMAND_LINE);
    }

    #[test]
    fn snapshot_of_the_marketplace() {
        insta::assert_snapshot!(artefact(paths::MARKETPLACE));
    }
}
