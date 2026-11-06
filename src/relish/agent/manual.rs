//! `relish agent manual`: the manual compiled into this binary, for an agent.
//!
//! The manual is about 95,000 tokens, far too much to read in one go, so an
//! agent searches it (`search`) and then reads one section (`show`). Both
//! work on sections: a heading and everything under it, with the anchor
//! GitHub gives that heading, so `diagnostics#relish-wtf-whats-broken-and-why`
//! names the same place here, on GitHub and on the website.
//!
//! The manual is our own text, not a workload's, so nothing here is wrapped
//! as untrusted. Like `source`, the commands are plain functions over a list
//! of chapters, so the tests run them over a small fixture.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Serialize;

use super::budget::{self, Rendered};
use super::envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, NextStep};
use super::fit;
use crate::relish::manual::assets::{self, Chapter};
use crate::relish::manual::{find_chapter, slug};

/// How many hits `search` returns at most.
pub const MAX_HITS: usize = 10;

/// The longest an excerpt line may be, in bytes.
const EXCERPT_LINE_BYTES: usize = 160;

/// The `relish agent manual` commands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ManualCommand {
    /// Up to ten sections that mention every word, best first, each with a
    /// short excerpt and the `show` command that prints it.
    Search {
        /// The words to look for; case doesn't matter.
        #[arg(required = true, num_args = 1..)]
        words: Vec<String>,
    },
    /// One chapter, or one section of it, as Markdown: `diagnostics` or
    /// `diagnostics#relish-path-can-a-reach-b`.
    Show {
        /// A chapter (its short name, part of it, or part of its title),
        /// optionally followed by `#` and a section's anchor.
        target: String,
    },
}

impl ManualCommand {
    /// The name the envelope's `command` field carries.
    pub fn name(&self) -> CommandName {
        match self {
            ManualCommand::Search { .. } => CommandName::ManualSearch,
            ManualCommand::Show { .. } => CommandName::ManualShow,
        }
    }
}

/// One heading of a chapter and the lines under it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    /// 1 for `#`, 2 for `##`, and so on.
    level: u8,
    /// The heading's text, without the `#`s.
    heading: String,
    /// GitHub's anchor for the heading.
    anchor: String,
    /// The heading's line, counting from 0.
    start: usize,
    /// The line after this section's own text: the next heading of any
    /// level, or the end of the chapter.
    body_end: usize,
    /// The line after this section and its subsections: the next heading at
    /// this level or above, or the end of the chapter.
    end: usize,
}

/// The sections of `markdown`, in order. A `#` line inside a fenced code
/// block is a shell comment, not a heading.
fn sections(markdown: &str) -> Vec<Section> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut headings: Vec<(usize, u8, String)> = Vec::new();
    let mut fence: Option<&str> = None;
    for (number, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker));
        match (fence, marker) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(open), Some(marker)) if open == marker => fence = None,
            (None, None) => {
                if let Some((level, heading)) = heading_of(line) {
                    headings.push((number, level, heading));
                }
            }
            _ => {}
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    headings
        .iter()
        .enumerate()
        .map(|(index, (start, level, heading))| {
            let later = &headings[index + 1..];
            let body_end = later.first().map_or(lines.len(), |(line, ..)| *line);
            let end = later
                .iter()
                .find(|(_, other, _)| other <= level)
                .map_or(lines.len(), |(line, ..)| *line);
            Section {
                level: *level,
                heading: heading.clone(),
                anchor: unique_anchor(&mut seen, anchor_of(heading)),
                start: *start,
                body_end,
                end,
            }
        })
        .collect()
}

/// `line` as an ATX heading: its level and text.
fn heading_of(line: &str) -> Option<(u8, String)> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    let rest = &line[hashes..];
    if !(1..=6).contains(&hashes) || !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    // `hashes` is at most 6, so it fits in a u8.
    Some((hashes as u8, text.to_string()))
}

/// The anchor GitHub gives a heading: lower case, letters, digits, `-` and
/// `_` kept, each space a `-`, everything else (backticks, colons,
/// apostrophes) dropped.
fn anchor_of(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// `anchor`, or `anchor-1`, `anchor-2`… when an earlier heading took it, as
/// GitHub numbers repeats.
fn unique_anchor(seen: &mut HashMap<String, usize>, anchor: String) -> String {
    let count = seen.entry(anchor.clone()).or_insert(0);
    let unique = match *count {
        0 => anchor,
        n => format!("{anchor}-{n}"),
    };
    *count += 1;
    unique
}

/// The `show` command for a chapter, or one of its sections.
fn show_command(chapter: &Chapter, anchor: Option<&str>) -> String {
    match anchor {
        Some(anchor) => format!("relish agent manual show {}#{anchor}", slug(&chapter.file)),
        None => format!("relish agent manual show {}", slug(&chapter.file)),
    }
}

/// `data` for `relish agent manual search`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchData {
    /// The words searched for, lower case.
    pub words: Vec<String>,
    /// How many sections mention every word; `hits` holds the best ten.
    pub total_hits: usize,
    /// The best sections, best first.
    pub hits: Vec<SearchHit>,
}

/// One section that mentions every word.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchHit {
    /// The chapter's short name, as `show` takes it.
    pub chapter: String,
    /// The chapter's title.
    pub title: String,
    /// The section's heading.
    pub heading: String,
    /// The section's anchor.
    pub anchor: String,
    /// The command that prints the section.
    pub show: String,
    /// One or two lines from the section where the words appear.
    pub excerpt: String,
    /// How well it matched; higher is better. A word in the heading counts
    /// ten times one in the text.
    pub score: u32,
}

/// Search `chapters` for sections mentioning every one of `words`.
fn search(chapters: &[Chapter], words: &[String]) -> Result<SearchData, AgentError> {
    let words: Vec<String> = words
        .iter()
        .flat_map(|word| word.split_whitespace())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return Err(AgentError::new(
            ErrorCode::Usage,
            "search needs at least one word",
        ));
    }
    let phrase = words.join(" ");
    let mut hits: Vec<(u32, usize, SearchHit)> = Vec::new();
    for (order, chapter) in chapters.iter().enumerate() {
        let lines: Vec<&str> = chapter.markdown.lines().collect();
        for section in sections(&chapter.markdown) {
            let heading = section.heading.to_lowercase();
            let body = lines[section.start + 1..section.body_end]
                .join("\n")
                .to_lowercase();
            if !words
                .iter()
                .all(|word| heading.contains(word.as_str()) || body.contains(word.as_str()))
            {
                continue;
            }
            let count = |text: &str, word: &str| text.matches(word).count() as u32;
            let mut score: u32 = words
                .iter()
                .map(|word| 10 * count(&heading, word) + count(&body, word))
                .sum();
            if words.len() > 1 {
                score += 20 * count(&heading, &phrase) + 5 * count(&body, &phrase);
            }
            let excerpt = excerpt(&lines[section.start + 1..section.body_end], &words);
            hits.push((
                score,
                order,
                SearchHit {
                    chapter: slug(&chapter.file).to_string(),
                    title: chapter.title.clone(),
                    show: show_command(chapter, Some(&section.anchor)),
                    heading: section.heading,
                    anchor: section.anchor,
                    excerpt,
                    score,
                },
            ));
        }
    }
    // Best first; among equals, in reading order (the sort is stable).
    hits.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    let total_hits = hits.len();
    Ok(SearchData {
        words,
        total_hits,
        hits: hits
            .into_iter()
            .take(MAX_HITS)
            .map(|(.., hit)| hit)
            .collect(),
    })
}

/// The line of `body` that mentions the most of `words` (the first such),
/// and the next line with text on it, each cut at [`EXCERPT_LINE_BYTES`].
fn excerpt(body: &[&str], words: &[String]) -> String {
    let texty: Vec<&str> = body
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with("```") && !line.starts_with("~~~"))
        .collect();
    let mentions = |line: &str| {
        let line = line.to_lowercase();
        words
            .iter()
            .filter(|word| line.contains(word.as_str()))
            .count()
    };
    let Some(best) =
        (0..texty.len()).max_by_key(|index| (mentions(texty[*index]), usize::MAX - index))
    else {
        return String::new();
    };
    texty[best..]
        .iter()
        .take(2)
        .map(|line| cut(line, EXCERPT_LINE_BYTES))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text`, cut at a character boundary at or below `max` bytes, ending in
/// `…[cut]` when it was cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let keep = text.floor_char_boundary(max.saturating_sub(budget::STRING_CUT_MARKER.len()));
    format!("{}{}", &text[..keep], budget::STRING_CUT_MARKER)
}

/// `data` for `relish agent manual show`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShowData {
    /// The chapter's short name.
    pub chapter: String,
    /// The chapter's file in `docs/manual/`.
    pub file: String,
    /// The chapter's title.
    pub title: String,
    /// The section's heading, or `null` for the whole chapter.
    pub heading: Option<String>,
    /// The section's anchor, or `null` for the whole chapter.
    pub anchor: Option<String>,
    /// The first and last line shown, counting from 1 in `file`.
    pub lines: LineRange,
    /// `false` when the Markdown was cut at `lines.to` to fit the byte
    /// budget; `next` names the subsections that would fit.
    pub complete: bool,
    /// The Markdown of the chapter or section, subsections included.
    pub markdown: String,
    /// The headings under the one shown, with the commands that show them.
    pub subsections: Vec<Subsection>,
}

/// A range of lines, counting from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct LineRange {
    /// The first line.
    pub from: usize,
    /// The last line.
    pub to: usize,
}

/// A heading inside the section shown.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Subsection {
    /// 2 for `##`, 3 for `###`, and so on.
    pub level: u8,
    /// The heading's text.
    pub heading: String,
    /// Its anchor.
    pub anchor: String,
    /// The command that shows it.
    pub show: String,
    /// The line it starts on, counting from 1.
    pub line: usize,
}

/// What `show` found, before it's fitted to the budget.
struct Shown<'a> {
    chapter: &'a Chapter,
    section: Option<Section>,
    lines: Vec<&'a str>,
    /// The line the shown part starts on, counting from 0.
    start: usize,
    subsections: Vec<Subsection>,
}

impl Shown<'_> {
    /// The envelope with the first `kept` lines. `whole` is the size of the
    /// uncut answer, when it's too big.
    fn envelope(&self, build: &BuildInfo, kept: usize, whole: Option<usize>) -> Envelope<ShowData> {
        let complete = kept == self.lines.len();
        let to = self.start + kept.max(1);
        let data = ShowData {
            chapter: slug(&self.chapter.file).to_string(),
            file: self.chapter.file.clone(),
            title: self.chapter.title.clone(),
            heading: self.section.as_ref().map(|section| section.heading.clone()),
            anchor: self.section.as_ref().map(|section| section.anchor.clone()),
            lines: LineRange {
                from: self.start + 1,
                to,
            },
            complete,
            markdown: self.lines[..kept].join("\n"),
            subsections: self.subsections.clone(),
        };
        let mut envelope = Envelope::answered(CommandName::ManualShow, build.clone(), data);
        if complete {
            return envelope;
        }
        envelope = envelope.mark_truncated().with_warning(format!(
            "/data/markdown: kept lines {}-{to} of {}-{} to fit the byte budget",
            self.start + 1,
            self.start + 1,
            self.start + self.lines.len()
        ));
        // The subsections that start after the cut are what the reader
        // missed; showing one of them is the cheap way to read on.
        for subsection in self
            .subsections
            .iter()
            .filter(|subsection| subsection.line > to)
            .take(3)
        {
            envelope = envelope.with_next(NextStep {
                run: subsection.show.clone(),
                why: format!("the cut part: {}", subsection.heading),
                read_only: true,
            });
        }
        if let Some(whole) = whole {
            let target = match &self.section {
                Some(section) => show_command(self.chapter, Some(&section.anchor)),
                None => show_command(self.chapter, None),
            };
            envelope = envelope.with_next(NextStep {
                run: format!("{target} --max-bytes {}", fit::round_up_to_kib(whole)),
                why: format!("all of it; the whole answer is {whole} bytes"),
                read_only: true,
            });
        }
        envelope
    }
}

/// Find what `target` (`chapter` or `chapter#anchor`) names.
fn find<'a>(chapters: &'a [Chapter], target: &str) -> Result<Shown<'a>, AgentError> {
    let (name, anchor) = match target.split_once('#') {
        Some((name, anchor)) => (name, Some(anchor.trim().to_lowercase())),
        None => (target, None),
    };
    // Take `07_diagnostics.md` and `07_diagnostics` as well as `diagnostics`.
    let name = slug(name.trim());
    let index = find_chapter(chapters, name).map_err(|_| {
        let names: Vec<&str> = chapters.iter().map(|chapter| slug(&chapter.file)).collect();
        AgentError::new(
            ErrorCode::NotFound,
            format!("no single manual chapter matches {name:?}"),
        )
        .with_hint(format!("chapters: {}", names.join(", ")))
    })?;
    let chapter = &chapters[index];
    let lines: Vec<&str> = chapter.markdown.lines().collect();
    let all = sections(&chapter.markdown);
    let (section, start, end) = match anchor.as_deref() {
        None | Some("") => (None, 0, lines.len()),
        Some(anchor) => {
            let Some(section) = all.iter().find(|section| section.anchor == anchor) else {
                return Err(unknown_anchor(chapter, &all, anchor));
            };
            (Some(section.clone()), section.start, section.end)
        }
    };
    let level = section.as_ref().map_or(1, |section| section.level);
    let subsections = all
        .iter()
        .filter(|other| other.start > start && other.start < end && other.level > level)
        .map(|other| Subsection {
            level: other.level,
            heading: other.heading.clone(),
            anchor: other.anchor.clone(),
            show: show_command(chapter, Some(&other.anchor)),
            line: other.start + 1,
        })
        .collect();
    // Trailing blank lines say nothing.
    let mut shown = lines[start..end].to_vec();
    while shown.last().is_some_and(|line| line.trim().is_empty()) {
        shown.pop();
    }
    Ok(Shown {
        chapter,
        section,
        lines: shown,
        start,
        subsections,
    })
}

/// The `not_found` failure for an anchor, naming the closest ones.
fn unknown_anchor(chapter: &Chapter, all: &[Section], anchor: &str) -> AgentError {
    let anchors: Vec<String> = all.iter().map(|section| section.anchor.clone()).collect();
    let close: Vec<String> = crate::relish::reader::fuzzy_rank(anchor, &anchors)
        .into_iter()
        .take(5)
        .map(|index| format!("{}#{}", slug(&chapter.file), anchors[index]))
        .collect();
    let hint = if close.is_empty() {
        format!(
            "{} lists the chapter's sections",
            show_command(chapter, None)
        )
    } else {
        format!("did you mean {}?", close.join(", "))
    };
    AgentError::new(
        ErrorCode::NotFound,
        format!("{} has no section #{anchor}", chapter.file),
    )
    .with_hint(hint)
}

/// Answer one `relish agent manual` command over `chapters`.
pub fn answer(
    chapters: &[Chapter],
    command: &ManualCommand,
    build: BuildInfo,
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    let max_bytes = max_bytes.max(budget::MIN_MAX_BYTES);
    match command {
        ManualCommand::Search { words } => match search(chapters, words) {
            Ok(data) => {
                let next = data.hits.first().map(|hit| NextStep {
                    run: hit.show.clone(),
                    why: format!("the best match: {}", hit.heading),
                    read_only: true,
                });
                let mut envelope = Envelope::answered(CommandName::ManualSearch, build, data);
                if let Some(next) = next {
                    envelope = envelope.with_next(next);
                }
                budget::render(&envelope, max_bytes, rerun)
            }
            Err(error) => failure(command.name(), build, error, max_bytes, rerun),
        },
        ManualCommand::Show { target } => match find(chapters, target) {
            Ok(shown) => {
                let total = shown.lines.len();
                let whole = fit::printed_length(&shown.envelope(&build, total, None));
                if whole <= max_bytes {
                    return budget::render(&shown.envelope(&build, total, None), max_bytes, rerun);
                }
                let kept = fit::largest_that_fits(total, |kept| {
                    fit::printed_length(&shown.envelope(&build, kept, Some(whole))) <= max_bytes
                })
                .unwrap_or(0);
                budget::render(&shown.envelope(&build, kept, Some(whole)), max_bytes, rerun)
            }
            Err(error) => failure(command.name(), build, error, max_bytes, rerun),
        },
    }
}

fn failure(
    command: CommandName,
    build: BuildInfo,
    error: AgentError,
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    let envelope: Envelope<()> = Envelope::failed(command, build, error);
    budget::render(&envelope, max_bytes, rerun)
}

/// Run one `relish agent manual` command over the embedded manual.
pub fn run(command: &ManualCommand, max_bytes: usize, rerun: &str) -> Rendered {
    answer(
        &assets::chapters(),
        command,
        BuildInfo::current(),
        max_bytes,
        rerun,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::agent::{DEFAULT_MAX_BYTES, ExitStatus};

    fn chapter(file: &str, markdown: &str) -> Chapter {
        Chapter {
            file: file.to_string(),
            title: markdown
                .lines()
                .find_map(|line| line.strip_prefix("# "))
                .unwrap_or(file)
                .to_string(),
            markdown: markdown.to_string(),
        }
    }

    fn fixture() -> Vec<Chapter> {
        vec![
            chapter(
                "01_deploy-an-app.md",
                "# Deploy an app\n\nApps roll out one instance at a time.\n\n\
                 ## Rolling deploys\n\nA rolling deploy replaces instances in batches.\n\
                 Each batch waits for health checks.\n\n\
                 ```sh\n# not a heading\nrelish apply app.toml\n```\n\n\
                 ### Rollback\n\nA failed rolling deploy rolls back.\n\n\
                 ## Health checks\n\nA deploy waits for them.\n",
            ),
            chapter(
                "07_diagnostics.md",
                "# Diagnostics\n\n## `relish wtf`: what's broken and why\n\n\
                 It explains a stuck deploy.\n\n## Examples\n\nOne.\n\n## Examples\n\nTwo.\n",
            ),
        ]
    }

    fn build() -> BuildInfo {
        BuildInfo {
            version: "0.2.7".to_string(),
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
            dirty: Some(false),
            source_digest: None,
        }
    }

    fn run_json(
        chapters: &[Chapter],
        command: ManualCommand,
        max_bytes: usize,
    ) -> (serde_json::Value, ExitStatus) {
        let rendered = answer(
            chapters,
            &command,
            build(),
            max_bytes,
            "relish agent manual",
        );
        assert!(rendered.json.len() <= max_bytes, "over budget");
        (serde_json::from_str(&rendered.json).unwrap(), rendered.exit)
    }

    fn search_for(words: &str) -> ManualCommand {
        ManualCommand::Search {
            words: words.split_whitespace().map(str::to_string).collect(),
        }
    }

    fn show(target: &str) -> ManualCommand {
        ManualCommand::Show {
            target: target.to_string(),
        }
    }

    #[test]
    fn anchors_match_github() {
        assert_eq!(
            anchor_of("`relish wtf`: what's broken and why"),
            "relish-wtf-whats-broken-and-why"
        );
        assert_eq!(anchor_of("Rolling deploys"), "rolling-deploys");
        assert_eq!(
            anchor_of("`-o json` and `--max-bytes`"),
            "-o-json-and---max-bytes"
        );
        let anchors: Vec<String> = sections(&fixture()[1].markdown)
            .into_iter()
            .map(|section| section.anchor)
            .collect();
        assert_eq!(
            anchors,
            [
                "diagnostics",
                "relish-wtf-whats-broken-and-why",
                "examples",
                "examples-1"
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_code_block_is_not_a_heading() {
        let headings: Vec<String> = sections(&fixture()[0].markdown)
            .into_iter()
            .map(|section| section.heading)
            .collect();
        assert_eq!(
            headings,
            [
                "Deploy an app",
                "Rolling deploys",
                "Rollback",
                "Health checks"
            ]
        );
    }

    #[test]
    fn search_ranks_a_heading_match_first_and_needs_every_word() {
        let (value, exit) = run_json(&fixture(), search_for("Rolling deploy"), DEFAULT_MAX_BYTES);
        assert_eq!(exit, ExitStatus::Answered);
        let hits = value["data"]["hits"].as_array().unwrap();
        assert_eq!(hits[0]["anchor"], "rolling-deploys");
        assert!(
            hits.iter().all(|hit| hit["heading"] != "Health checks"),
            "it mentions deploy but not rolling: {hits:?}"
        );
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn search_with_no_hits_still_answers() {
        let (value, exit) = run_json(&fixture(), search_for("kubernetes"), DEFAULT_MAX_BYTES);
        assert_eq!(exit, ExitStatus::Answered);
        assert_eq!(value["data"]["total_hits"], 0);
        assert_eq!(value["next"], serde_json::json!([]));
    }

    #[test]
    fn search_returns_at_most_ten_hits() {
        let markdown: String = (0..15)
            .map(|index| format!("## Part {index}\n\nThe needle.\n\n"))
            .collect();
        let chapters = vec![chapter("01_many.md", &format!("# Many\n\n{markdown}"))];
        let (value, _) = run_json(&chapters, search_for("needle"), DEFAULT_MAX_BYTES);
        assert_eq!(value["data"]["total_hits"], 15);
        assert_eq!(value["data"]["hits"].as_array().unwrap().len(), MAX_HITS);
    }

    #[test]
    fn show_prints_one_section_with_its_subsections() {
        let (value, exit) = run_json(
            &fixture(),
            show("deploy#rolling-deploys"),
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(exit, ExitStatus::Answered);
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn show_takes_the_file_name_too() {
        for target in ["07_diagnostics", "07_diagnostics.md", "diag", "diagnostics"] {
            let (value, exit) = run_json(&fixture(), show(target), DEFAULT_MAX_BYTES);
            assert_eq!(exit, ExitStatus::Answered, "{target}");
            assert_eq!(value["data"]["chapter"], "diagnostics");
            assert_eq!(value["data"]["heading"], serde_json::Value::Null);
        }
    }

    #[test]
    fn an_unknown_chapter_or_anchor_is_not_found_with_a_hint() {
        let (value, exit) = run_json(&fixture(), show("networking"), DEFAULT_MAX_BYTES);
        assert_eq!(exit, ExitStatus::Failed);
        assert_eq!(value["error"]["code"], "not_found");
        assert_eq!(
            value["error"]["hint"],
            "chapters: deploy-an-app, diagnostics"
        );
        let (value, _) = run_json(&fixture(), show("deploy#rolling"), DEFAULT_MAX_BYTES);
        assert_eq!(value["error"]["code"], "not_found");
        assert!(
            value["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("deploy-an-app#rolling-deploys"),
            "{value}"
        );
    }

    #[test]
    fn a_long_section_is_cut_at_a_line_and_says_where() {
        let body: String = (0..400)
            .map(|index| format!("Line {index} of a long section about ingress.\n"))
            .collect();
        let chapters = vec![chapter(
            "03_networking.md",
            &format!("# Networking\n\n{body}\n## Ingress TLS\n\nCertificates.\n"),
        )];
        let (value, exit) = run_json(&chapters, show("networking"), 4096);
        assert_eq!(exit, ExitStatus::Answered);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["data"]["complete"], false);
        let markdown = value["data"]["markdown"].as_str().unwrap();
        assert!(
            markdown.ends_with("ingress."),
            "cut at a line: {markdown:?}"
        );
        assert_eq!(
            value["next"][0]["run"],
            "relish agent manual show networking#ingress-tls"
        );
        assert!(
            value["next"][1]["run"]
                .as_str()
                .unwrap()
                .starts_with("relish agent manual show networking --max-bytes "),
            "{value}"
        );
    }

    #[test]
    fn the_real_manual_parses_into_unique_anchors_and_answers_searches() {
        let chapters = assets::chapters();
        for chapter in &chapters {
            let all = sections(&chapter.markdown);
            assert_eq!(
                all.first().map(|section| section.level),
                Some(1),
                "{}",
                chapter.file
            );
            let mut anchors: Vec<&str> =
                all.iter().map(|section| section.anchor.as_str()).collect();
            let count = anchors.len();
            anchors.sort();
            anchors.dedup();
            assert_eq!(anchors.len(), count, "{}", chapter.file);
        }
        let (value, _) = run_json(&chapters, search_for("rolling deploy"), DEFAULT_MAX_BYTES);
        assert!(value["data"]["total_hits"].as_u64().unwrap() > 0);
        let (value, exit) = run_json(
            &chapters,
            show("diagnostics#relish-wtf-whats-broken-and-why"),
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(exit, ExitStatus::Answered, "{value}");
        assert!(
            value["data"]["markdown"]
                .as_str()
                .unwrap()
                .starts_with("## `relish wtf`")
        );
    }

    #[test]
    fn every_search_hit_shows_with_its_own_command() {
        let chapters = fixture();
        let (value, _) = run_json(&chapters, search_for("deploy"), DEFAULT_MAX_BYTES);
        for hit in value["data"]["hits"].as_array().unwrap() {
            let target = hit["show"]
                .as_str()
                .unwrap()
                .strip_prefix("relish agent manual show ")
                .unwrap();
            let (shown, exit) = run_json(&chapters, show(target), DEFAULT_MAX_BYTES);
            assert_eq!(exit, ExitStatus::Answered, "{target}");
            assert_eq!(shown["data"]["heading"], hit["heading"]);
        }
    }
}
