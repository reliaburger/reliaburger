//! `relish agent guide`, and every agent-facing file made from the same
//! chapter.
//!
//! The manual chapter "Working with an AI agent" is the one source. `relish
//! agent guide` prints it from the copy embedded in the binary, and
//! [`generate`] turns it into the files an agent finds elsewhere: `llms.txt`
//! and the `/agents` page on the website, the `reliaburger` skill, the Claude
//! Code plugin and its marketplace, the website's skills discovery index and
//! its copy of the skill, `relish mcp`'s MCP Registry entry (`server.json`)
//! and MCP Bundle manifest,
//! and the generated block of the repository's `AGENTS.md`. A test in `tests/suite/agent_docs.rs` compares
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

use clap::Command;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;

use super::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, NextStep, agent_command};
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
    /// The website's skills discovery index, in the format of Cloudflare's
    /// [Agent Skills Discovery RFC](https://github.com/cloudflare/agent-skills-discovery-rfc).
    pub const SKILL_INDEX: &str = "docs/website/.well-known/agent-skills/index.json";
    /// The website's copy of the skill, which the index points at.
    pub const SITE_SKILL: &str = "docs/website/.well-known/agent-skills/reliaburger/SKILL.md";
    /// `relish mcp`'s entry for the official
    /// [MCP Registry](https://registry.modelcontextprotocol.io).
    pub const MCP_SERVER: &str = "server.json";
    /// The `manifest.json` release packaging puts in each platform's MCP
    /// Bundle (`.mcpb`) beside that platform's `relish`.
    pub const MCPB_MANIFEST: &str = "scripts/release/mcpb-manifest.json";
}

/// The `$schema` of the skills discovery index: version 0.2.0 of the RFC.
pub const SKILL_INDEX_SCHEMA: &str = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";

/// The `$schema` of `server.json`, the MCP Registry's format.
pub const MCP_SERVER_SCHEMA: &str =
    "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";

/// The [MCP Bundle](https://github.com/modelcontextprotocol/mcpb) manifest
/// format [`paths::MCPB_MANIFEST`] follows.
pub const MCPB_MANIFEST_VERSION: &str = "0.3";

/// Where a bundle keeps its `relish`, relative to the bundle's root.
pub const MCPB_ENTRY_POINT: &str = "server/relish";

/// The command that makes the read-only token a bundle's `token` setting
/// asks for. A day is enough for one session.
pub const READ_ONLY_TOKEN_COMMAND: &str =
    "relish token create --name agent --role read-only --ttl-days 1";

/// The bundle description's one line on credentials (#1695, decision (d)):
/// with the token setting empty, the bundle uses the current context, which
/// may hold an admin token.
pub const MCPB_ADMIN_ADVICE: &str = "With the token setting empty, relish uses its current \
context, which may hold an admin token, and warns on stderr when it does; make a read-only \
token with `relish token create --name agent --role read-only --ttl-days 1` and put it in \
the token setting.";

/// `relish mcp`'s name in the MCP Registry. The `io.github.reliaburger`
/// namespace is the one a GitHub login to the organisation proves.
pub const MCP_REGISTRY_NAME: &str = "io.github.reliaburger/relish";

/// The longest `description` the MCP Registry accepts.
pub const MCP_DESCRIPTION_MAX: usize = 100;

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
    /// What `--section` takes: the heading's anchor, such as `through-mcp`.
    pub id: String,
    /// The heading, without `## `. The first section has the chapter's
    /// title.
    pub heading: String,
    /// Everything under the heading, as Markdown.
    pub markdown: String,
}

/// One line of the chapter's table of contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ContentsEntry {
    /// What `--section` takes: the heading's anchor, such as `through-mcp`.
    pub id: String,
    /// The heading, without `## `.
    pub heading: String,
    /// The size of the section's Markdown, in bytes.
    pub bytes: usize,
}

/// How much of the chapter an answer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuideScope {
    /// Every section: the whole chapter fitted the budget.
    Whole,
    /// The contents and the sections to read first: the whole chapter
    /// didn't fit the budget, and `next` says how to read the rest.
    Outline,
    /// The one section `--section` asked for.
    Section,
}

/// `data` for `relish agent guide`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GuideData {
    /// The manual chapter this is, e.g. `16_working-with-an-ai-agent.md`.
    pub chapter: String,
    /// The chapter's title.
    pub title: String,
    /// Whether `sections` is the whole chapter, an outline's first
    /// sections, or the one section asked for.
    pub scope: GuideScope,
    /// An outline's table of contents: every section of the chapter, in
    /// order, with its id and size. Empty otherwise: the whole chapter has
    /// every section, and a `section` answer follows an outline.
    pub contents: Vec<ContentsEntry>,
    /// The sections this answer holds, in order. Only a `truncated`
    /// section answer cuts one, at a paragraph break.
    pub sections: Vec<GuideSection>,
    /// Every `relish agent` command this binary has, from its own CLI
    /// definition. Empty unless `scope` is `whole`: the `commands` section
    /// has the same table.
    pub commands: Vec<CommandSummary>,
}

/// `relish agent guide`'s arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct GuideArgs {
    /// Only this section, by its id in `contents` (such as `the-envelope`)
    /// or its heading.
    #[arg(long)]
    pub section: Option<String>,
}

/// The sections an outline holds, in order, when they fit: how to read an
/// answer, follow `next`, treat `untrusted` text as data and work
/// read-only, then the promises every command keeps.
pub const READ_FIRST: &[&str] = &["in-brief", "the-contract"];

/// The section an outline's `next` offers after [`READ_FIRST`]: every
/// field of the envelope.
pub const READ_NEXT: &str = "the-envelope";

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
            let heading = std::mem::replace(&mut heading, next.trim().to_string());
            sections.push(GuideSection {
                id: slug(&heading),
                heading,
                markdown: std::mem::take(&mut body).trim().to_string(),
            });
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    sections.push(GuideSection {
        id: slug(&heading),
        heading,
        markdown: body.trim().to_string(),
    });
    (title, sections)
}

/// `data` for `relish agent guide` from a chapter and the CLI definition:
/// the whole chapter.
pub fn guide_data(chapter: &str, agent: &Command) -> GuideData {
    let (title, sections) = sections(chapter);
    GuideData {
        chapter: CHAPTER_FILE.to_string(),
        title,
        scope: GuideScope::Whole,
        contents: sections
            .iter()
            .map(|section| ContentsEntry {
                id: section.id.clone(),
                heading: section.heading.clone(),
                bytes: section.markdown.len(),
            })
            .collect(),
        sections,
        commands: commands(agent),
    }
}

/// The envelope `relish agent guide` prints for the whole of `chapter`.
pub fn guide_envelope(chapter: &str, agent: &Command, relish: BuildInfo) -> Envelope<GuideData> {
    let data = GuideData {
        contents: Vec::new(),
        ..guide_data(chapter, agent)
    };
    Envelope::answered(CommandName::Guide, relish, data).with_next(NextStep {
        run: "relish agent source version".to_string(),
        why: "which source this relish carries, and whether the nodes run it".to_string(),
        read_only: true,
    })
}

/// Where in `sections` the one `wanted` names is, by id or heading, in any
/// case, with or without a leading `#`.
fn find_section(sections: &[GuideSection], wanted: &str) -> Option<usize> {
    let wanted = slug(wanted.trim().trim_start_matches('#'));
    sections.iter().position(|section| section.id == wanted)
}

/// A `next` step that reads the section `id`.
fn read_section(id: &str, why: String) -> NextStep {
    NextStep {
        run: format!("relish agent guide --section {id}"),
        why,
        read_only: true,
    }
}

/// The envelope for `relish agent guide --section wanted`, or why there's
/// no such section.
///
/// A section bigger than `max_bytes` is cut at the last paragraph break
/// that fits, outside code blocks, never inside a sentence: the answer is
/// marked `truncated`, a warning says how much it kept, and the first
/// `next` step asks for the whole section.
fn section_envelope(
    data: GuideData,
    wanted: &str,
    relish: BuildInfo,
    max_bytes: usize,
) -> Result<Envelope<GuideData>, AgentError> {
    let Some(index) = find_section(&data.sections, wanted) else {
        let ids: Vec<&str> = data
            .contents
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        return Err(AgentError::new(
            ErrorCode::NotFound,
            format!("the guide has no section {wanted:?}"),
        )
        .with_hint(format!("sections: {}", ids.join(", "))));
    };
    let following = data.contents.get(index + 1).cloned();
    let section = data.sections[index].clone();
    // The contents came with the outline; a section answer leaves them out
    // so the section has the budget to itself.
    let make = |markdown: &str, whole: Option<usize>| {
        let one = GuideData {
            chapter: data.chapter.clone(),
            title: data.title.clone(),
            scope: GuideScope::Section,
            contents: Vec::new(),
            sections: vec![GuideSection {
                markdown: markdown.to_string(),
                ..section.clone()
            }],
            commands: Vec::new(),
        };
        let mut envelope = Envelope::answered(CommandName::Guide, relish.clone(), one);
        if let Some(whole) = whole {
            envelope = envelope
                .mark_truncated()
                .with_warning(format!(
                    "guide_section_cut: {} is {} bytes and this is its first {}, up to a \
                     paragraph break",
                    section.id,
                    section.markdown.len(),
                    markdown.len()
                ))
                .with_next(NextStep {
                    run: format!(
                        "relish agent guide --section {} --max-bytes {}",
                        section.id,
                        whole.div_ceil(1024) * 1024
                    ),
                    why: format!("the whole section, {whole} bytes"),
                    read_only: true,
                });
        }
        if let Some(entry) = &following {
            envelope = envelope.with_next(read_section(
                &entry.id,
                format!(
                    "the next section, {:?}, {} bytes",
                    entry.heading, entry.bytes
                ),
            ));
        }
        envelope
    };
    let whole = make(&section.markdown, None);
    let size = rendered_length(&whole);
    if size <= max_bytes {
        return Ok(whole);
    }
    let cut = paragraph_breaks(&section.markdown)
        .into_iter()
        .rev()
        .map(|end| make(&section.markdown[..end], Some(size)))
        .find(|envelope| rendered_length(envelope) <= max_bytes);
    Ok(cut.unwrap_or_else(|| make("", Some(size))))
}

/// The byte offsets of the blank lines in `markdown` outside code blocks:
/// the places a section can be cut between paragraphs.
fn paragraph_breaks(markdown: &str) -> Vec<usize> {
    let mut breaks = Vec::new();
    let mut offset: usize = 0;
    let mut in_fence = false;
    for line in markdown.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.trim().is_empty() {
            // Cut before the blank line, so the kept part ends with text.
            breaks.push(offset.saturating_sub(1));
        }
        offset += line.len();
    }
    breaks
}

/// One line of JSON's length, newline included, as [`render`] counts it.
fn rendered_length<T: Serialize>(envelope: &Envelope<T>) -> usize {
    serde_json::to_vec(envelope).map_or(usize::MAX, |json| json.len() + 1)
}

/// The outline of the chapter, for a budget too small for all of it: the
/// contents, as many of [`READ_FIRST`] as fit whole, and `next` steps to
/// the ones that didn't and to [`READ_NEXT`]. `whole` is the whole
/// answer's size in bytes.
fn outline_envelope(
    data: GuideData,
    relish: BuildInfo,
    whole: usize,
    max_bytes: usize,
) -> Envelope<GuideData> {
    let warning = format!(
        "guide_outline: the whole chapter is {whole} bytes, more than this budget of \
         {max_bytes}, so this is its contents and the sections to read first, each whole; \
         read another with --section ID, or all of it with --max-bytes {}",
        whole.div_ceil(1024) * 1024
    );
    let build = |sections: Vec<GuideSection>| {
        let mut steps = Vec::new();
        for id in READ_FIRST {
            let kept = sections.iter().any(|section| section.id == *id);
            if let (false, Some(entry)) = (kept, data.contents.iter().find(|e| e.id == *id)) {
                steps.push(read_section(
                    id,
                    format!(
                        "read this first, {} bytes; it didn't fit this budget",
                        entry.bytes
                    ),
                ));
            }
        }
        steps.push(read_section(
            READ_NEXT,
            "how to read every answer, field by field; --section takes any id in data.contents"
                .to_string(),
        ));
        let outline = GuideData {
            scope: GuideScope::Outline,
            sections,
            commands: Vec::new(),
            ..data.clone()
        };
        let envelope = Envelope::answered(CommandName::Guide, relish.clone(), outline)
            .with_warning(warning.clone());
        steps.into_iter().fold(envelope, Envelope::with_next)
    };
    let mut kept: Vec<GuideSection> = Vec::new();
    for id in READ_FIRST {
        let Some(section) = data.sections.iter().find(|section| section.id == *id) else {
            continue;
        };
        let mut tried = kept.clone();
        tried.push(section.clone());
        if rendered_length(&build(tried.clone())) <= max_bytes {
            kept = tried;
        }
    }
    build(kept)
}

/// The answer to `relish agent guide` over `chapter`: the one section
/// `section` names; else the whole chapter when it fits `max_bytes`; else
/// its outline, which holds only whole sections.
pub fn answer(
    chapter: &str,
    agent: &Command,
    relish: BuildInfo,
    section: Option<&str>,
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    let max_bytes = max_bytes.max(super::MIN_MAX_BYTES);
    if let Some(wanted) = section {
        let data = guide_data(chapter, agent);
        return match section_envelope(data, wanted, relish.clone(), max_bytes) {
            Ok(envelope) => render(&envelope, max_bytes, rerun),
            Err(error) => render(
                &Envelope::<()>::failed(CommandName::Guide, relish, error),
                max_bytes,
                rerun,
            ),
        };
    }
    let whole = guide_envelope(chapter, agent, relish.clone());
    let size = rendered_length(&whole);
    if size <= max_bytes {
        return render(&whole, max_bytes, rerun);
    }
    let outline = outline_envelope(guide_data(chapter, agent), relish, size, max_bytes);
    render(&outline, max_bytes, rerun)
}

/// Run `relish agent guide` against the chapter embedded in this binary.
pub fn run(max_bytes: usize, section: Option<&str>, rerun: &str) -> Rendered {
    let Some(asset) = crate::relish::manual::assets::ManualAssets::get(CHAPTER_FILE) else {
        let error = AgentError::new(
            ErrorCode::Internal,
            format!("this relish was built without the manual chapter {CHAPTER_FILE}"),
        );
        return super::failure(CommandName::Guide, error, max_bytes, rerun);
    };
    let chapter = String::from_utf8_lossy(asset.data.as_ref());
    answer(
        &chapter,
        &agent_command(),
        BuildInfo::current(),
        section,
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
    /// Every website page's Markdown copy, for `llms.txt`.
    pub site: &'a [SitePage],
}

/// A website page's Markdown copy, as `llms.txt` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitePage {
    /// The copy's path on the site, e.g. `compare/nomad.md`.
    pub markdown: String,
    /// The page's `<title>`.
    pub title: String,
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
    let skill = skill_md(&summary, &brief, &table)?;

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
            contents: llms_txt(&summary, &brief, &title, inputs),
        },
        Artefact {
            path: paths::AGENTS_PAGE_MARKDOWN,
            contents: format!("{}\n\n{page}", generated_note()),
        },
        Artefact {
            path: paths::AGENTS_PAGE_HTML,
            contents: agents_html(&title, &summary, &page, inputs.version),
        },
        Artefact {
            path: paths::SKILL_INDEX,
            contents: skill_index_json(&skill_description(&summary)?, &skill),
        },
        Artefact {
            path: paths::SITE_SKILL,
            contents: skill.clone(),
        },
        Artefact {
            path: paths::SKILL,
            contents: skill,
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
        Artefact {
            path: paths::MCP_SERVER,
            contents: mcp_server_json(agent, inputs.version),
        },
        Artefact {
            path: paths::MCPB_MANIFEST,
            contents: mcpb_manifest_json(agent, inputs.version),
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

fn llms_txt(summary: &str, brief: &str, title: &str, inputs: &Inputs<'_>) -> String {
    let version = inputs.version;
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
    for entry in inputs
        .manual
        .iter()
        .filter(|entry| entry.file != CHAPTER_FILE)
    {
        let _ = writeln!(out, "- [{}]({raw}/docs/manual/{})", entry.title, entry.file);
    }
    if !inputs.site.is_empty() {
        out.push_str("\n## Website\n\nEvery page on reliaburger.com, as Markdown.\n\n");
        for page in inputs.site {
            let _ = writeln!(out, "- [{}]({SITE}/{})", page.title, page.markdown);
        }
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

/// `text` percent-encoded for a URL query value: everything but the
/// unreserved characters of RFC 3986 becomes `%XX`.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// One-click links that add `relish mcp` to an agent client, each made from
/// the same name and command line `relish agent setup` writes. Each client
/// shows the server and asks before it adds anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLinks {
    /// Cursor's `cursor://` deeplink.
    pub cursor: String,
    /// The same, through cursor.com, for pages that strip custom schemes.
    pub cursor_web: String,
    /// VS Code's `vscode:mcp/install` link.
    pub vscode: String,
    /// The same, through vscode.dev, for pages that strip custom schemes.
    pub vscode_web: String,
    /// Goose's `goose://extension` link.
    pub goose: String,
    /// The Claude Code command that adds the same server.
    pub claude_code: String,
}

/// The install links for `relish mcp`.
pub fn install_links() -> InstallLinks {
    use base64::Engine as _;
    let name = super::setup::MCP_SERVER_NAME;
    let mut words = super::setup::MCP_COMMAND_LINE.iter().copied();
    let command = words.next().unwrap_or("relish");
    let args: Vec<&str> = words.collect();

    let cursor_config = base64::engine::general_purpose::STANDARD
        .encode(json!({"command": command, "args": args}).to_string());
    let cursor_query = format!("name={name}&config={}", percent_encode(&cursor_config));
    let vscode = format!(
        "vscode:mcp/install?{}",
        percent_encode(&json!({"name": name, "command": command, "args": args}).to_string())
    );
    let mut goose = format!("goose://extension?cmd={}", percent_encode(command));
    for arg in &args {
        let _ = write!(goose, "&arg={}", percent_encode(arg));
    }
    let _ = write!(
        goose,
        "&id={name}&name={}&description={}",
        percent_encode("Reliaburger"),
        percent_encode(super::setup::MCP_DESCRIPTION)
    );
    InstallLinks {
        cursor: format!("cursor://anysphere.cursor-deeplink/mcp/install?{cursor_query}"),
        cursor_web: format!("https://cursor.com/en/install-mcp?{cursor_query}"),
        vscode_web: format!(
            "https://insiders.vscode.dev/redirect?url={}",
            percent_encode(&vscode)
        ),
        vscode,
        goose,
        claude_code: format!(
            "claude mcp add {name} -- {}",
            super::setup::MCP_COMMAND_LINE.join(" ")
        ),
    }
}

/// A heading's anchor, the way GitHub makes one: lower case, spaces to `-`,
/// punctuation other than `-` and `_` dropped.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

fn markdown_to_html(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(markdown, Options::ENABLE_TABLES));
    out
}

/// The box near the top of `/agents` with the install buttons, the Claude
/// Code line and what they need.
fn install_box(version: &str) -> String {
    let links = install_links();
    format!(
        r##"<aside class="agents-install" id="add-to-your-agent" aria-labelledby="add-to-your-agent-title">
<h2 id="add-to-your-agent-title">Add it to your agent <a class="permalink" href="#add-to-your-agent" aria-label="Link to the install buttons">#</a></h2>
<p>Each button adds one MCP server, <code>{name}</code>, that runs <code>{line}</code>: read-only tools over stdio, with no port, no trust flag and no auto-approval. Your client shows the entry and asks before it adds it. Install <code>relish</code> {version} or later first (<code>relish agent version</code> says which you have); the server runs the <code>relish</code> on your <code>PATH</code>.</p>
<p class="agents-install-buttons"><a class="install-button" href="{cursor}">Add to Cursor</a><a class="install-button" href="{vscode}">Add to VS Code</a><a class="install-button" href="{goose}">Add to Goose</a></p>
<p>Claude Code, from a shell:</p>
<pre><code data-install-command>{claude}</code></pre>
<p>Or let <code>relish agent setup</code> find every agent on this machine and show the change before it writes anything (see <a href="#wiring-up-your-agent">Wiring up your agent</a>). Give the server a read-only token: see <a href="#a-read-only-token">A read-only token</a>.</p>
</aside>
"##,
        name = escape_html(super::setup::MCP_SERVER_NAME),
        line = escape_html(&super::setup::MCP_COMMAND_LINE.join(" ")),
        version = escape_html(version),
        cursor = escape_html(&links.cursor),
        vscode = escape_html(&links.vscode),
        goose = escape_html(&links.goose),
        claude = escape_html(&links.claude_code),
    )
}

/// The page's body: the intro, the install box, a list of the sections, and
/// each `## ` section as a `<section>` whose `id` is its heading's slug, so
/// `/agents#how-reliaburger-is-tested` links to one and CSS can style it.
fn agents_body(page: &str, version: &str) -> String {
    let (title, sections) = sections(page);
    let mut sections = sections.into_iter();
    let mut body = format!("<h1>{}</h1>\n", escape_html(&title));
    if let Some(intro) = sections.next() {
        body.push_str(&markdown_to_html(&intro.markdown));
    }
    body.push_str(&install_box(version));
    let sections: Vec<GuideSection> = sections.collect();
    body.push_str("<nav class=\"agents-contents\" aria-label=\"On this page\">\n<h2>On this page</h2>\n<ol>\n");
    for section in &sections {
        let _ = writeln!(
            body,
            "<li><a href=\"#{}\">{}</a></li>",
            slug(&section.heading.replace('`', "")),
            inline_html(&section.heading)
        );
    }
    body.push_str("</ol>\n</nav>\n");
    for section in &sections {
        let id = slug(&section.heading.replace('`', ""));
        let _ = write!(
            body,
            "<section class=\"agents-section\" id=\"{id}\">\n<h2>{heading} <a class=\"permalink\" href=\"#{id}\" aria-label=\"Link to this section\">#</a></h2>\n{content}</section>\n",
            heading = inline_html(&section.heading),
            content = markdown_to_html(&section.markdown),
        );
    }
    body
}

/// A heading's Markdown as inline HTML, without the paragraph around it.
fn inline_html(markdown: &str) -> String {
    let html = markdown_to_html(markdown);
    html.trim()
        .trim_start_matches("<p>")
        .trim_end_matches("</p>")
        .to_string()
}

fn agents_html(title: &str, summary: &str, page: &str, version: &str) -> String {
    let body = agents_body(page, version);
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
    <footer><span>Reliaburger · by {AUTHOR}</span><a href="./agents.md">This page as Markdown</a><span class="footer-note">Generated from the manual chapter {title_escaped}. Static HTML and CSS; the site’s copy script adds the copy buttons. No tracking or third-party requests.</span></footer>
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
/// `relish agent setup` writes into an agent's own config, with the command
/// line's default budget. The plugin is Claude Code's, with a window for
/// 24 KiB answers; `relish mcp`'s own 8 KiB is for small local models, and
/// setup's clients include Goose, which runs them (#1687).
fn plugin_mcp_json() -> String {
    let mut words = super::setup::MCP_COMMAND_LINE.iter();
    let command = words.next().copied().unwrap_or("relish");
    let budget = super::budget::DEFAULT_MAX_BYTES.to_string();
    let mut args: Vec<&str> = words.copied().collect();
    args.extend(["--max-bytes", budget.as_str()]);
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

/// The SHA-256 of `bytes`, as the skills index writes a digest.
fn sha256_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// `/.well-known/agent-skills/index.json`: one `skill-md` entry, the
/// website's copy of the skill, with the digest of its exact bytes. A client
/// checks the digest before it installs the skill, so the index changes
/// whenever the skill does.
fn skill_index_json(description: &str, skill: &str) -> String {
    pretty_json(&json!({
        "$schema": SKILL_INDEX_SCHEMA,
        "skills": [{
            "name": SKILL_NAME,
            "type": "skill-md",
            "description": description,
            "url": format!("/.well-known/agent-skills/{SKILL_NAME}/SKILL.md"),
            "digest": sha256_digest(skill.as_bytes()),
        }],
    }))
}

/// What the registry entry says `relish mcp` is, within
/// [`MCP_DESCRIPTION_MAX`] characters.
pub const MCP_DESCRIPTION: &str =
    "Read-only Reliaburger cluster diagnosis: the relish agent commands as MCP tools.";

/// The tools `relish mcp` serves by default, with their descriptions.
fn served_tools(agent: &Command) -> Vec<(String, String)> {
    use crate::relish::mcp::tools;
    tools::derive(agent.clone(), &tools::data_schema)
        .into_iter()
        .map(|spec| {
            let description = spec
                .tool
                .description
                .as_deref()
                .unwrap_or_default()
                .to_string();
            (spec.tool.name.to_string(), description)
        })
        .collect()
}

/// `server.json` for the MCP Registry.
///
/// The committed file has no `packages` entry: a package's `fileSha256` is
/// the digest of a release's `.mcpb` bundle, which only exists once the
/// release is built. Promotion writes the entry it publishes, with one
/// `mcpb` package per platform (`scripts/release/mcp_registry.py`). Until
/// then the entry says what the server is, how to start it and which tools
/// it has, in the publisher-provided `_meta` the schema leaves open, and
/// gives the install line.
fn mcp_server_json(agent: &Command, version: &str) -> String {
    let tools: Vec<String> = served_tools(agent)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    pretty_json(&json!({
        "$schema": MCP_SERVER_SCHEMA,
        "name": MCP_REGISTRY_NAME,
        "title": "Reliaburger relish",
        "description": MCP_DESCRIPTION,
        "version": version,
        "websiteUrl": format!("{SITE}/agents"),
        "repository": {"url": REPOSITORY, "source": "github"},
        "_meta": {
            "io.modelcontextprotocol.registry/publisher-provided": {
                "install": format!("curl -fsSL {SITE}/install.sh | sh -s -- --install-only"),
                "command": super::setup::MCP_COMMAND_LINE,
                "transport": "stdio",
                "readOnly": true,
                "tools": tools,
                "documentation": format!(
                    "{REPOSITORY}/blob/v{version}/{chapter}",
                    chapter = paths::CHAPTER
                ),
            },
        },
    }))
}

/// The [MCP Bundle](https://github.com/modelcontextprotocol/mcpb)
/// `manifest.json` for `relish mcp`.
///
/// Release packaging (`scripts/release/package.py`) zips it with one
/// platform's `relish` at [`MCPB_ENTRY_POINT`], sets `version` to the
/// release's and narrows `compatibility.platforms` to that platform. A
/// client that installs the bundle runs the `relish` inside it, so the
/// command is relative to the bundle's directory, never a `relish` on the
/// `PATH`.
///
/// It asks for nothing by default. Two optional settings, an endpoint and a
/// read-only token, reach `relish` as `RELIABURGER_ENDPOINT` and
/// `RELIABURGER_TOKEN`. A client passes a setting the user left blank as
/// an empty string, and `relish` treats an empty variable as unset, so an
/// unconfigured bundle uses `relish`'s current context (#1695).
fn mcpb_manifest_json(agent: &Command, version: &str) -> String {
    let tools: Vec<serde_json::Value> = served_tools(agent)
        .into_iter()
        .map(|(name, description)| json!({"name": name, "description": description}))
        .collect();
    let args: Vec<&str> = super::setup::MCP_COMMAND_LINE
        .iter()
        .skip(1)
        .copied()
        .collect();
    pretty_json(&json!({
        "manifest_version": MCPB_MANIFEST_VERSION,
        "name": "relish",
        "display_name": "Reliaburger relish",
        "version": version,
        "description": MCP_DESCRIPTION,
        "long_description": format!("{MCP_DESCRIPTION}\n\n{MCPB_ADMIN_ADVICE}"),
        "author": {"name": AUTHOR, "url": SITE},
        "repository": {"type": "git", "url": REPOSITORY},
        "homepage": format!("{SITE}/agents"),
        "documentation": format!(
            "{REPOSITORY}/blob/v{version}/{chapter}",
            chapter = paths::CHAPTER
        ),
        "support": format!("{REPOSITORY}/issues"),
        "license": LICENSE,
        "keywords": ["reliaburger", "relish", "orchestrator", "containers", "sre"],
        "server": {
            "type": "binary",
            "entry_point": MCPB_ENTRY_POINT,
            "mcp_config": {
                "command": format!("${{__dirname}}/{MCPB_ENTRY_POINT}"),
                "args": args,
                "env": {
                    "RELIABURGER_ENDPOINT": "${user_config.endpoint}",
                    "RELIABURGER_TOKEN": "${user_config.token}",
                },
            },
        },
        "user_config": {
            "endpoint": {
                "type": "string",
                "title": "Cluster API endpoint (optional)",
                "description": "The Bun API address, such as https://10.0.0.5:9117. \
                    Leave it empty to use relish's current context.",
                "required": false,
                "default": "",
            },
            "token": {
                "type": "string",
                "title": "Read-only API token (optional)",
                "description": format!(
                    "A read-only token, made with `{READ_ONLY_TOKEN_COMMAND}`: \
                     {REPOSITORY}/blob/main/docs/manual/{CHAPTER_FILE}#a-read-only-token. \
                     Leave it empty to use relish's current context."
                ),
                "sensitive": true,
                "required": false,
                "default": "",
            },
        },
        "tools": tools,
        "compatibility": {"platforms": ["darwin", "linux"]},
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

    fn fixture_site() -> Vec<SitePage> {
        vec![
            SitePage {
                markdown: "index.md".to_string(),
                title: "Reliaburger".to_string(),
            },
            SitePage {
                markdown: "compare/nomad.md".to_string(),
                title: "Reliaburger compared with Nomad".to_string(),
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
            site: &fixture_site(),
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
                site: &fixture_site(),
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
            if [paths::PLUGIN_MCP, paths::MCP_SERVER, paths::MCPB_MANIFEST].contains(&artefact.path)
            {
                // Server entries, with no room for prose; their own tests
                // check them.
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
                site: &fixture_site(),
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
        let rendered = run(max_bytes, None, "relish agent guide");
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
    /// its default budget (80 KiB, not 24) returns every section in one
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
    /// one call. When chapter 16 outgrew 64 KiB, the maintainer chose to
    /// keep it whole and raise `GUIDE_MAX_BYTES` to 80 KiB (8 Nov 2026).
    /// Outgrowing that is the same choice again, made deliberately.
    #[test]
    fn the_agent_chapter_fits_the_guides_default_budget() {
        let rendered = run(1024 * 1024, None, "relish agent guide");
        let whole = rendered.json.len();
        assert!(
            whole <= super::super::GUIDE_MAX_BYTES,
            "relish agent guide is {whole} bytes, over its {} byte default: split \
             {CHAPTER_FILE} or raise GUIDE_MAX_BYTES deliberately",
            super::super::GUIDE_MAX_BYTES
        );
    }

    /// The guide answered at `max_bytes`, for `section` if given, as JSON.
    fn guide_at(max_bytes: usize, section: Option<&str>) -> serde_json::Value {
        let rendered = run(max_bytes, section, "relish agent guide");
        serde_json::from_str(&rendered.json).unwrap()
    }

    /// Every section's Markdown in the embedded chapter, by id.
    fn embedded_sections() -> Vec<GuideSection> {
        let asset = crate::relish::manual::assets::ManualAssets::get(CHAPTER_FILE).unwrap();
        sections(&String::from_utf8_lossy(asset.data.as_ref())).1
    }

    /// A budget smaller than the chapter used to drop sections from the
    /// end, which at MCP's 8 KiB left a small model the introduction and
    /// half the brief. Now it answers with the contents, each section with
    /// its size, and the sections to read first, each whole, and `next`
    /// names the section after them (#1687).
    #[test]
    fn a_budget_smaller_than_the_chapter_answers_with_its_outline() {
        let whole = guide_at(1024 * 1024, None);
        assert_eq!(whole["data"]["scope"], "whole");
        assert_eq!(whole["data"]["contents"], serde_json::json!([]));
        let all = embedded_sections();
        for budget in [8 * 1024, super::super::DEFAULT_MAX_BYTES] {
            let value = guide_at(budget, None);
            assert_eq!(value["ok"], true);
            assert_eq!(value["data"]["scope"], "outline", "{budget}");
            assert_eq!(value["truncated"], false, "nothing is cut: {budget}");
            let kept: Vec<&str> = value["data"]["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|section| section["id"].as_str().unwrap())
                .collect();
            assert_eq!(kept, READ_FIRST, "{budget}");
            for section in value["data"]["sections"].as_array().unwrap() {
                let id = section["id"].as_str().unwrap();
                let original = all.iter().find(|s| s.id == id).unwrap();
                assert_eq!(
                    section["markdown"],
                    original.markdown.as_str(),
                    "whole: {id}"
                );
            }
            assert_eq!(
                value["next"][0]["run"],
                format!("relish agent guide --section {READ_NEXT}")
            );
            let warning = value["warnings"][0].as_str().unwrap();
            assert!(warning.starts_with("guide_outline: "), "{warning}");
            assert!(value.to_string().len() < budget);
        }
    }

    /// The table of contents lists every section once, in order, with the
    /// size of its Markdown.
    #[test]
    fn the_contents_list_every_section_with_its_size() {
        let value = guide_at(8 * 1024, None);
        let contents = value["data"]["contents"].as_array().unwrap();
        let all = embedded_sections();
        assert_eq!(contents.len(), all.len());
        for (entry, section) in contents.iter().zip(&all) {
            assert_eq!(entry["id"], section.id.as_str());
            assert_eq!(entry["heading"], section.heading.as_str());
            assert_eq!(entry["bytes"], section.markdown.len());
        }
        let ids: Vec<&str> = all.iter().map(|section| section.id.as_str()).collect();
        for id in READ_FIRST.iter().chain([&READ_NEXT]) {
            assert!(ids.contains(id), "{id} isn't a section: {ids:?}");
        }
        let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "section ids repeat: {ids:?}");
    }

    /// When a budget is too small for a section to read first, the outline
    /// leaves it out whole, keeps any later one that fits, and its `next`
    /// asks for the one left out, by name, rather than cutting it.
    #[test]
    fn a_section_to_read_first_that_doesnt_fit_is_named_in_next() {
        let value = guide_at(4 * 1024, None);
        assert_eq!(value["data"]["scope"], "outline");
        assert_eq!(value["truncated"], false);
        let kept: Vec<&str> = value["data"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|section| section["id"].as_str().unwrap())
            .collect();
        assert_eq!(kept, ["the-contract"]);
        let runs: Vec<&str> = value["next"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["run"].as_str().unwrap())
            .collect();
        assert_eq!(
            runs,
            [
                "relish agent guide --section in-brief",
                "relish agent guide --section the-envelope",
            ]
        );
    }

    /// `--section` takes an id or a heading, in any case, answers with that
    /// one section whole, and offers the next one.
    #[test]
    fn a_section_is_found_by_id_or_heading_and_offers_the_next() {
        for wanted in ["through-mcp", "Through MCP", "#through-mcp"] {
            let value = guide_at(8 * 1024, Some(wanted));
            assert_eq!(value["ok"], true, "{wanted}");
            assert_eq!(value["data"]["scope"], "section");
            assert_eq!(value["data"]["commands"], serde_json::json!([]));
            let sections = value["data"]["sections"].as_array().unwrap();
            assert_eq!(sections.len(), 1);
            assert_eq!(sections[0]["heading"], "Through MCP");
            assert_eq!(
                value["next"][0]["run"],
                "relish agent guide --section wiring-up-your-agent"
            );
        }
    }

    /// The last section has no next one to offer.
    #[test]
    fn the_last_section_offers_no_next_section() {
        let last = embedded_sections().pop().unwrap();
        let value = guide_at(8 * 1024, Some(&last.id));
        assert_eq!(value["data"]["sections"][0]["id"], last.id.as_str());
        assert_eq!(value["next"], serde_json::json!([]));
    }

    /// An unknown section is `not_found`, and the hint lists every id.
    #[test]
    fn an_unknown_section_is_not_found_and_lists_the_ids() {
        let rendered = run(8 * 1024, Some("no-such-section"), "relish agent guide");
        assert_ne!(rendered.exit, super::super::ExitStatus::Answered);
        let value: serde_json::Value = serde_json::from_str(&rendered.json).unwrap();
        assert_eq!(value["error"]["code"], "not_found");
        let hint = value["error"]["hint"].as_str().unwrap();
        assert!(
            hint.contains("in-brief") && hint.contains("through-mcp"),
            "{hint}"
        );
    }

    /// A section bigger than its budget is cut between paragraphs, never
    /// inside a sentence, and says so: `truncated`, a warning with what it
    /// kept, and a first `next` step that asks for the whole section.
    #[test]
    fn a_section_bigger_than_its_budget_is_cut_between_paragraphs_and_says_so() {
        let original = embedded_sections()
            .into_iter()
            .find(|section| section.id == "through-mcp")
            .unwrap()
            .markdown;
        for budget in [1024, 2048, 4096] {
            let value = guide_at(budget, Some("through-mcp"));
            assert!(value.to_string().len() < budget, "{budget}");
            assert_eq!(value["truncated"], true, "{budget}");
            let kept = value["data"]["sections"][0]["markdown"].as_str().unwrap();
            assert!(original.starts_with(kept), "{budget}: a prefix");
            assert!(
                kept.is_empty() || original[kept.len()..].starts_with("\n\n"),
                "{budget}: cut at a paragraph break: {kept:?}"
            );
            let warning = value["warnings"][0].as_str().unwrap();
            assert!(
                warning.starts_with("guide_section_cut: through-mcp is "),
                "{warning}"
            );
            assert!(
                value["next"][0]["run"]
                    .as_str()
                    .unwrap()
                    .starts_with("relish agent guide --section through-mcp --max-bytes "),
                "{}",
                value["next"]
            );
        }
        let kept = guide_at(4096, Some("through-mcp"))["data"]["sections"][0]["markdown"]
            .as_str()
            .unwrap()
            .len();
        assert!(kept > 1024, "4 KiB keeps most of what fits: {kept}");
    }

    /// A blank line inside a code block isn't a paragraph break.
    #[test]
    fn paragraph_breaks_skip_code_blocks() {
        let markdown = "One.\n\n```sh\na\n\nb\n```\n\nTwo.";
        let breaks = paragraph_breaks(markdown);
        let cuts: Vec<&str> = breaks.iter().map(|&end| &markdown[..end]).collect();
        assert_eq!(cuts, ["One.", "One.\n\n```sh\na\n\nb\n```"]);
    }

    /// Every section of the chapter fits MCP's 8 KiB default whole, so
    /// following an outline's `next` never meets a cut section.
    #[test]
    fn every_section_fits_mcps_default_budget_whole() {
        for section in embedded_sections() {
            let value = guide_at(8 * 1024, Some(&section.id));
            assert_eq!(value["truncated"], false, "{} is cut at 8 KiB", section.id);
        }
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

    /// Undo [`percent_encode`], for checking what a link carries.
    fn percent_decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn query_values<'a>(link: &'a str, key: &str) -> Vec<&'a str> {
        let query = &link[link.find('?').unwrap() + 1..];
        query
            .split('&')
            .filter_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .collect()
    }

    #[test]
    fn every_install_link_starts_the_server_setup_registers() {
        use base64::Engine as _;
        let links = install_links();
        let expected = super::super::setup::MCP_COMMAND_LINE;
        let name = super::super::setup::MCP_SERVER_NAME;

        for cursor in [&links.cursor, &links.cursor_web] {
            assert_eq!(query_values(cursor, "name"), [name]);
            let config = percent_decode(query_values(cursor, "config")[0]);
            let config = base64::engine::general_purpose::STANDARD
                .decode(config)
                .unwrap();
            let config: serde_json::Value = serde_json::from_slice(&config).unwrap();
            assert_eq!(config["command"], expected[0]);
            assert_eq!(config["args"], json!(expected[1..]));
        }
        assert!(
            links
                .cursor
                .starts_with("cursor://anysphere.cursor-deeplink/mcp/install?")
        );

        let vscode: serde_json::Value = serde_json::from_str(&percent_decode(
            links.vscode.strip_prefix("vscode:mcp/install?").unwrap(),
        ))
        .unwrap();
        assert_eq!(vscode["name"], name);
        assert_eq!(vscode["command"], expected[0]);
        assert_eq!(vscode["args"], json!(expected[1..]));
        let redirected = query_values(&links.vscode_web, "url")[0];
        assert_eq!(percent_decode(redirected), links.vscode);

        assert!(links.goose.starts_with("goose://extension?"));
        let mut goose = vec![percent_decode(query_values(&links.goose, "cmd")[0])];
        goose.extend(
            query_values(&links.goose, "arg")
                .into_iter()
                .map(percent_decode),
        );
        assert_eq!(goose, expected);
        assert_eq!(query_values(&links.goose, "id"), [name]);

        assert_eq!(
            links.claude_code,
            format!("claude mcp add {name} -- {}", expected.join(" "))
        );
    }

    #[test]
    fn slugs_follow_github_s_anchors() {
        assert_eq!(
            slug("How Reliaburger is tested"),
            "how-reliaburger-is-tested"
        );
        assert_eq!(
            slug("Text the cluster didn't write"),
            "text-the-cluster-didnt-write"
        );
        assert_eq!(slug("Reading an app's logs"), "reading-an-apps-logs");
    }

    #[test]
    fn the_agents_page_has_a_linkable_section_per_heading_and_the_install_box() {
        let html = artefact(paths::AGENTS_PAGE_HTML);
        for id in ["in-brief", "commands", "sending-us-a-fix"] {
            assert!(
                html.contains(&format!("<section class=\"agents-section\" id=\"{id}\">")),
                "no section {id}"
            );
            assert!(
                html.contains(&format!("<li><a href=\"#{id}\">")),
                "{id} not listed"
            );
            assert!(html.contains(&format!("<a class=\"permalink\" href=\"#{id}\"")));
        }
        let links = install_links();
        for link in [&links.cursor, &links.vscode, &links.goose] {
            assert!(html.contains(&format!("href=\"{}\"", escape_html(link))));
        }
        assert!(html.contains(&format!(
            "<code data-install-command>{}</code>",
            links.claude_code
        )));
        assert!(html.contains("<code>relish</code> 0.2.7 or later"));
        // The install box comes after the intro and before the first section.
        let install = html.find("id=\"add-to-your-agent\"").unwrap();
        assert!(html.find("Intro paragraph.").unwrap() < install);
        assert!(install < html.find("id=\"in-brief\"").unwrap());
    }

    #[test]
    fn snapshot_of_the_skill() {
        insta::assert_snapshot!(artefact(paths::SKILL));
    }

    #[test]
    fn snapshot_of_the_plugin_manifest() {
        insta::assert_snapshot!(artefact(paths::PLUGIN));
    }

    /// The plugin starts the same server `relish agent setup` registers,
    /// with the command line's 24 KiB budget: Claude Code has the window
    /// for it, and the server's own 8 KiB is for small local models (#1687).
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
        let mut expected = super::super::setup::MCP_COMMAND_LINE.to_vec();
        expected.extend(["--max-bytes", "24576"]);
        assert_eq!(line, expected);
        assert_eq!(super::super::budget::DEFAULT_MAX_BYTES, 24576);
    }

    #[test]
    fn snapshot_of_the_marketplace() {
        insta::assert_snapshot!(artefact(paths::MARKETPLACE));
    }

    #[test]
    fn llms_txt_links_every_website_copy() {
        let llms = artefact(paths::LLMS_TXT);
        assert!(llms.contains("\n## Website\n"));
        assert!(llms.contains("- [Reliaburger](https://reliaburger.com/index.md)"));
        assert!(llms.contains(
            "- [Reliaburger compared with Nomad](https://reliaburger.com/compare/nomad.md)"
        ));
    }

    #[test]
    fn the_website_serves_the_plugins_skill_byte_for_byte() {
        assert_eq!(artefact(paths::SITE_SKILL), artefact(paths::SKILL));
    }

    #[test]
    fn the_skill_index_names_the_skill_with_the_digest_of_its_bytes() {
        let index: serde_json::Value = serde_json::from_str(&artefact(paths::SKILL_INDEX)).unwrap();
        assert_eq!(index["$schema"], SKILL_INDEX_SCHEMA);
        let skills = index["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        let entry = &skills[0];
        assert_eq!(entry["name"], SKILL_NAME);
        assert_eq!(entry["type"], "skill-md");
        assert_eq!(
            entry["url"], "/.well-known/agent-skills/reliaburger/SKILL.md",
            "served beside the index, where the site copy is"
        );
        assert!(
            paths::SITE_SKILL.ends_with(entry["url"].as_str().unwrap()),
            "the URL is the site copy's path"
        );
        let skill = artefact(paths::SITE_SKILL);
        assert_eq!(entry["digest"], sha256_digest(skill.as_bytes()));
        let digest = entry["digest"].as_str().unwrap();
        let hex = digest.strip_prefix("sha256:").unwrap();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // The RFC says the description should match the skill's own.
        let front = skill.split("\n---\n").next().unwrap();
        let description: String = front
            .lines()
            .find_map(|line| line.strip_prefix("description: "))
            .map(|quoted| serde_json::from_str(quoted).unwrap())
            .unwrap();
        assert_eq!(entry["description"], description.as_str());
    }

    #[test]
    fn a_changed_skill_changes_the_index_digest() {
        let manual = fixture_manual();
        let edited = FIXTURE_CHAPTER.replace("first.**", "first, always.**");
        let changed = generate(
            &Inputs {
                chapter: &edited,
                agents_md: FIXTURE_AGENTS_MD,
                manual: &manual,
                version: "0.2.7",
                site: &fixture_site(),
            },
            &fixture_agent(),
        )
        .unwrap();
        let stale = stale_paths(&changed, |path| Some(artefact(path)));
        assert!(stale.contains(&paths::SKILL_INDEX), "{stale:?}");
        assert!(stale.contains(&paths::SITE_SKILL), "{stale:?}");
    }

    /// The registry entry describes the server `relish mcp` really is: the
    /// same command line `relish agent setup` registers, over stdio, with
    /// exactly the tools the server offers by default.
    #[test]
    fn the_registry_entry_lists_exactly_the_tools_relish_mcp_serves() {
        let entry = generate(
            &Inputs {
                chapter: FIXTURE_CHAPTER,
                agents_md: FIXTURE_AGENTS_MD,
                manual: &fixture_manual(),
                version: "0.2.7",
                site: &[],
            },
            &agent_command(),
        )
        .unwrap()
        .into_iter()
        .find(|artefact| artefact.path == paths::MCP_SERVER)
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&entry.contents).unwrap();
        assert_eq!(value["name"], MCP_REGISTRY_NAME);
        assert_eq!(value["version"], "0.2.7");
        assert!(MCP_DESCRIPTION.chars().count() <= MCP_DESCRIPTION_MAX);
        assert!(
            value.get("packages").is_none(),
            "promotion adds the packages, once the bundles exist"
        );
        let provided = &value["_meta"]["io.modelcontextprotocol.registry/publisher-provided"];
        assert_eq!(provided["transport"], "stdio");
        assert_eq!(provided["readOnly"], true);
        let command: Vec<&str> = provided["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|word| word.as_str().unwrap())
            .collect();
        assert_eq!(command, super::super::setup::MCP_COMMAND_LINE);
        let listed: Vec<&str> = provided["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool.as_str().unwrap())
            .collect();
        let server = crate::relish::mcp::Server::new(&Default::default()).unwrap();
        let served: Vec<String> = server
            .tools()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(listed, served);
        for excluded in crate::relish::mcp::tools::EXCLUDED {
            let name = crate::relish::mcp::tools::tool_name(excluded.command);
            assert!(!listed.contains(&name.as_str()), "{name} is excluded");
        }
    }

    #[test]
    fn snapshot_of_the_skill_index() {
        insta::assert_snapshot!(artefact(paths::SKILL_INDEX));
    }

    #[test]
    fn snapshot_of_the_registry_entry() {
        insta::assert_snapshot!(artefact(paths::MCP_SERVER));
    }

    /// The MCP Bundle's manifest starts the `relish` inside the bundle as
    /// `relish mcp`, over stdio, and lists exactly the tools that server
    /// offers by default, as the registry entry does.
    #[test]
    fn the_bundle_manifest_runs_the_bundled_relish_mcp_with_the_served_tools() {
        let manifest = generate(
            &Inputs {
                chapter: FIXTURE_CHAPTER,
                agents_md: FIXTURE_AGENTS_MD,
                manual: &fixture_manual(),
                version: "0.2.7",
                site: &[],
            },
            &agent_command(),
        )
        .unwrap()
        .into_iter()
        .find(|artefact| artefact.path == paths::MCPB_MANIFEST)
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&manifest.contents).unwrap();
        assert_eq!(value["manifest_version"], MCPB_MANIFEST_VERSION);
        assert_eq!(value["version"], "0.2.7");
        assert_eq!(value["description"], MCP_DESCRIPTION);
        let server = &value["server"];
        assert_eq!(server["type"], "binary");
        assert_eq!(server["entry_point"], MCPB_ENTRY_POINT);
        assert_eq!(
            server["mcp_config"]["command"],
            format!("${{__dirname}}/{MCPB_ENTRY_POINT}")
        );
        let args: Vec<&str> = server["mcp_config"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|word| word.as_str().unwrap())
            .collect();
        assert_eq!(args, &super::super::setup::MCP_COMMAND_LINE[1..]);
        let listed: Vec<&str> = value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let served = crate::relish::mcp::Server::new(&Default::default()).unwrap();
        let served: Vec<String> = served
            .tools()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(listed, served);
        assert!(
            value["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| !tool["description"].as_str().unwrap().is_empty())
        );
        assert_eq!(
            value["compatibility"]["platforms"],
            serde_json::json!(["darwin", "linux"]),
            "release packaging narrows this to one platform per bundle"
        );
    }

    /// #1695: the bundle asks for nothing by default. Its two settings are
    /// optional and empty, so a bundle nobody configured uses relish's
    /// current context, and the token one asks for a read-only token only.
    #[test]
    fn the_bundle_takes_an_optional_endpoint_and_read_only_token() {
        let value: serde_json::Value =
            serde_json::from_str(&artefact(paths::MCPB_MANIFEST)).unwrap();
        let settings = value["user_config"].as_object().unwrap();
        assert_eq!(
            settings.keys().map(String::as_str).collect::<Vec<_>>(),
            ["endpoint", "token"]
        );
        for (name, setting) in settings {
            assert_eq!(setting["type"], "string", "{name}");
            assert_eq!(setting["required"], false, "{name}");
            assert_eq!(setting["default"], "", "{name}");
            assert!(setting["title"].as_str().unwrap().contains("(optional)"));
            let words = format!("{} {}", setting["title"], setting["description"]);
            assert!(!words.to_lowercase().contains("admin"), "{name}: {words}");
        }
        let endpoint = &settings["endpoint"];
        assert!(endpoint.get("sensitive").is_none());
        assert!(
            endpoint["description"]
                .as_str()
                .unwrap()
                .contains("Leave it empty to use relish's current context")
        );
        let token = &settings["token"];
        assert_eq!(token["sensitive"], true);
        assert!(token["title"].as_str().unwrap().starts_with("Read-only"));
        let description = token["description"].as_str().unwrap();
        assert!(
            description.contains(READ_ONLY_TOKEN_COMMAND),
            "{description}"
        );
        assert!(description.contains(&format!(
            "{REPOSITORY}/blob/main/docs/manual/{CHAPTER_FILE}#a-read-only-token"
        )));
        assert_eq!(
            value["server"]["mcp_config"]["env"],
            serde_json::json!({
                "RELIABURGER_ENDPOINT": "${user_config.endpoint}",
                "RELIABURGER_TOKEN": "${user_config.token}",
            })
        );
    }

    /// #1695, decision (d): an unconfigured bundle uses the current context,
    /// which may hold an admin token, so the bundle's description says so in
    /// one line and says how to make a read-only one instead.
    #[test]
    fn the_bundle_description_warns_that_an_empty_token_may_mean_admin() {
        let value: serde_json::Value =
            serde_json::from_str(&artefact(paths::MCPB_MANIFEST)).unwrap();
        let long = value["long_description"].as_str().unwrap();
        let advice = long.lines().last().unwrap();
        assert!(advice.contains("admin"), "{advice}");
        assert!(advice.contains(READ_ONLY_TOKEN_COMMAND), "{advice}");
        assert!(advice.contains("token setting"), "{advice}");
        assert!(long.starts_with(MCP_DESCRIPTION), "{long}");
    }

    #[test]
    fn snapshot_of_the_bundle_manifest() {
        insta::assert_snapshot!(artefact(paths::MCPB_MANIFEST));
    }
}
