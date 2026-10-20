//! Doc snippet tests (#708): what people copy out of the docs must work.
//!
//! The manual (compiled into `relish`), `README.md`, `docs/README.md` and
//! `docs/whitepaper.md` hold dozens of shell and TOML blocks. Every one is
//! checked here against the real code:
//!
//! - In ```sh, ```bash, ```shell and ```console blocks, every `relish`
//!   invocation (also as `target/debug/relish`, `cargo run --bin relish --`,
//!   after `sudo`, inside `$(…)`) parses with `RELISH_PARSE_ONLY=1`.
//! - Every ```toml block parses with the type that reads it: workload tables
//!   (`[app.*]`, `[job.*]`, …) with `Config::parse` then `validate()`, node
//!   tables with `NodeConfig::parse` and the checks `bun` runs at startup, chaos scenarios with the scenario
//!   loader, batch manifests with the manifest type. A block that mixes
//!   workload and node tables is split and both halves are checked.
//!
//! A block that can't be checked says so on the line before its fence:
//!
//! - `<!-- snippet: fragment -->`: not complete enough to parse on its own.
//! - `<!-- snippet: planned #N -->`: shows a feature issue N will build.
//! - `<!-- snippet: pending #N -->`: shows CLI or config that issue N is
//!   adding. It is still checked, and must still FAIL: once issue N lands and
//!   the block parses, the test asks for the marker to go.
//!
//! `planned` and `pending` must name an issue in [`OPEN_ISSUES`]. Every
//! marker is counted and listed in the output, so none hides quietly.
//!
//! Prose names commands too: an inline `relish …` span outside a fence must
//! name a real subcommand, and every flag in it must be one that subcommand
//! takes. Spans are rarely complete commands, so they aren't parsed whole.
//! A span for a command that doesn't exist yet is either labelled
//! `*(planned)*` right after it, or listed in [`INLINE_PENDING`] with its
//! issue; either way it must still fail, so the label lapses when it lands.

use std::collections::BTreeMap;
use std::process::Command;

use super::website::read;

/// Issues a `planned` or `pending` marker may name. CI has no GitHub token,
/// so this is a static list: when one of these closes, remove it here and the
/// markers naming it fail until their snippets are made real.
const OPEN_ISSUES: &[(u32, &str)] = &[
    (362, "CA status and revocation"),
    (656, "drain: cordon, drain and uncordon"),
    (711, "Franchise multi-cluster"),
    (1209, "GPU metrics and relish gpus"),
    (1210, "relish fault gpu"),
    (1221, "model shorthand and relish model"),
    (1223, "group type"),
    (1238, "0.9.0 pipelines and 0.10.0 fair share, revisited after 0.8.0"),
];

/// Inline `relish …` spans in prose for commands an open issue is adding.
/// Each must still fail the check; when it passes, remove its entry.
const INLINE_PENDING: &[(&str, u32)] = &[
    ("relish ca status", 362),
    ("relish ca revoke", 362),
    ("relish drain", 656),
    ("relish franchise", 711),
    ("relish franchise join", 711),
    ("relish franchise status", 711),
    ("relish top --franchise", 711),
    ("relish gpus", 1209),
    ("relish fault gpu <node>/<device> --xid <code>", 1210),
    ("relish model run qwen3-32b", 1221),
    ("relish model run <name>", 1221),
    ("relish usage", 1238),
];

/// Chapters of the manual, which ships inside `relish`.
fn manual_files() -> Vec<String> {
    let directory = super::website::repository().join("docs/manual");
    let mut files: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".md"))
        .map(|name| format!("docs/manual/{name}"))
        .collect();
    files.sort();
    files
}

const OTHER_FILES: &[&str] = &["README.md", "docs/README.md", "docs/whitepaper.md"];

fn all_files() -> Vec<String> {
    let mut files = manual_files();
    files.extend(OTHER_FILES.iter().map(|file| file.to_string()));
    files
}

// --- Extraction --------------------------------------------------------------

/// What the line before a fence says about the block.
#[derive(Debug, Clone, PartialEq)]
enum Marker {
    Fragment,
    Planned(u32),
    Pending(u32),
    /// A `<!-- snippet: … -->` comment we don't understand. Reported as a
    /// failure rather than ignored, so a typo can't turn into a silent skip.
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Language {
    Shell,
    Console,
    Toml,
}

/// One fenced block that the checks read.
#[derive(Debug, Clone)]
struct Snippet {
    file: String,
    /// 1-based line of the opening fence.
    line: usize,
    language: Language,
    marker: Option<Marker>,
    body: String,
}

impl Snippet {
    fn location(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

fn parse_marker(line: &str) -> Option<Marker> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim()
        .strip_prefix("snippet:")?
        .trim();
    let issue = |rest: &str| rest.trim().strip_prefix('#')?.parse::<u32>().ok();
    let marker = match inner.split_once(' ') {
        None if inner == "fragment" => Marker::Fragment,
        Some(("planned", rest)) => {
            issue(rest).map_or_else(|| Marker::Unknown(inner.to_string()), Marker::Planned)
        }
        Some(("pending", rest)) => {
            issue(rest).map_or_else(|| Marker::Unknown(inner.to_string()), Marker::Pending)
        }
        _ => Marker::Unknown(inner.to_string()),
    };
    Some(marker)
}

/// Every checked fenced block in `text`, plus any marker that doesn't sit
/// right before one (reported as `(line, marker)`).
fn extract(file: &str, text: &str) -> (Vec<Snippet>, Vec<(usize, Marker)>) {
    let mut snippets = Vec::new();
    let mut orphans = Vec::new();
    // The marker on the last non-blank line, if that line was one.
    let mut pending_marker: Option<(usize, Marker)> = None;
    let mut open: Option<(usize, Option<Language>, Option<Marker>, String)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim();
        if let Some((start, language, marker, mut body)) = open.take() {
            if trimmed.starts_with("```") {
                if let Some(language) = language {
                    snippets.push(Snippet {
                        file: file.to_string(),
                        line: start,
                        language,
                        marker,
                        body,
                    });
                }
            } else {
                body.push_str(line);
                body.push('\n');
                open = Some((start, language, marker, body));
            }
            continue;
        }
        if let Some(info) = trimmed.strip_prefix("```") {
            let language = match info.trim() {
                "sh" | "bash" | "shell" => Some(Language::Shell),
                "console" => Some(Language::Console),
                "toml" => Some(Language::Toml),
                _ => None,
            };
            let marker = pending_marker.take().map(|(at, marker)| {
                if language.is_none() {
                    orphans.push((at, marker.clone()));
                }
                marker
            });
            open = Some((number, language, marker, String::new()));
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if let Some(previous) = pending_marker.take() {
            orphans.push(previous);
        }
        pending_marker = parse_marker(trimmed).map(|marker| (number, marker));
    }
    if let Some(previous) = pending_marker {
        orphans.push(previous);
    }
    (snippets, orphans)
}

/// Lines of a shell block as the shell would read them: `\` continuations
/// joined, a leading `$ ` prompt removed. In a console block only prompt
/// lines are commands; the rest is output.
fn logical_lines(snippet: &Snippet) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    for raw in snippet.body.lines() {
        let line = raw.trim();
        let text = match current.take() {
            Some(mut joined) => {
                joined.push(' ');
                joined.push_str(line);
                joined
            }
            None => match (line.strip_prefix("$ "), snippet.language) {
                (Some(command), _) => command.to_string(),
                (None, Language::Console) => continue,
                (None, _) => line.to_string(),
            },
        };
        match text.strip_suffix('\\') {
            Some(unfinished) => current = Some(unfinished.trim_end().to_string()),
            None => lines.push(text),
        }
    }
    lines.extend(current);
    lines
}

/// `<NODE>`, `<app-name>`: what readers replace. `1` reads as a name, an
/// id or a count, so any argument type accepts it.
fn fill_placeholders(line: &str) -> String {
    let placeholder = regex::Regex::new(r"<[A-Za-z][A-Za-z0-9_.:-]*>").unwrap();
    placeholder.replace_all(line, "1").into_owned()
}

/// Split one shell line into simple commands (separated by `|`, `&&`, `;`,
/// `&`, or nested in `$( … )`), each as its words with quotes removed.
/// A `$( … )` is its own command, and stands for `1` in the word around it.
/// Comments and redirections are dropped. This is a small subset of shell
/// grammar, enough for documentation.
fn simple_commands(line: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut in_double = false;
    // For each enclosing `$(`: whether it opened inside double quotes, and
    // the words of the command around it.
    let mut nesting: Vec<(bool, Vec<String>, Option<String>)> = Vec::new();
    let mut drop_next_word = false;
    let mut chars = line.chars().peekable();

    let flush_word = |word: &mut Option<String>, words: &mut Vec<String>, drop: &mut bool| {
        if let Some(finished) = word.take() {
            if *drop {
                *drop = false;
            } else {
                words.push(finished);
            }
        }
    };
    let flush_command = |words: &mut Vec<String>, commands: &mut Vec<Vec<String>>| {
        if !words.is_empty() {
            commands.push(std::mem::take(words));
        }
    };

    while let Some(character) = chars.next() {
        if character == '$' && chars.peek() == Some(&'(') {
            chars.next();
            nesting.push((in_double, std::mem::take(&mut words), word.take()));
            in_double = false;
            continue;
        }
        if in_double {
            match character {
                '"' => in_double = false,
                '\\' => word.get_or_insert_default().extend(chars.next()),
                other => word.get_or_insert_default().push(other),
            }
            continue;
        }
        match character {
            '"' => {
                in_double = true;
                word.get_or_insert_default();
            }
            '\'' => {
                let quoted = word.get_or_insert_default();
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    quoted.push(inner);
                }
            }
            '\\' => word.get_or_insert_default().extend(chars.next()),
            ')' if !nesting.is_empty() => {
                flush_word(&mut word, &mut words, &mut drop_next_word);
                flush_command(&mut words, &mut commands);
                if let Some((quoted, outer_words, outer_word)) = nesting.pop() {
                    in_double = quoted;
                    words = outer_words;
                    word = Some(outer_word.unwrap_or_default() + "1");
                }
            }
            '|' | ';' | '&' => {
                flush_word(&mut word, &mut words, &mut drop_next_word);
                flush_command(&mut words, &mut commands);
            }
            '>' | '<' => {
                // `2>&1`, `> out.json`, `<<EOF`: drop the file descriptor and
                // the target.
                if word
                    .as_deref()
                    .is_some_and(|w| w.chars().all(|c| c.is_ascii_digit()))
                {
                    word = None;
                }
                flush_word(&mut word, &mut words, &mut drop_next_word);
                while matches!(chars.peek(), Some('>' | '<' | '&')) {
                    chars.next();
                }
                drop_next_word = true;
            }
            '#' if word.is_none() => break,
            space if space.is_whitespace() => {
                flush_word(&mut word, &mut words, &mut drop_next_word);
            }
            other => word.get_or_insert_default().push(other),
        }
    }
    flush_word(&mut word, &mut words, &mut drop_next_word);
    flush_command(&mut words, &mut commands);
    commands
}

/// The arguments `relish` receives, if this simple command runs it.
fn relish_arguments(words: &[String]) -> Option<Vec<String>> {
    let is_assignment = |word: &str| {
        word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    let mut rest = words;
    loop {
        match rest {
            [first, tail @ ..] if first == "sudo" || first == "export" || is_assignment(first) => {
                rest = tail
            }
            [limactl, shell, _vm, tail @ ..] if limactl == "limactl" && shell == "shell" => {
                rest = tail
            }
            _ => break,
        }
    }
    if let [cargo, run, tail @ ..] = rest
        && cargo == "cargo"
        && run == "run"
    {
        let runs_relish = tail
            .windows(2)
            .any(|pair| pair[0] == "--bin" && pair[1] == "relish");
        if !runs_relish {
            return None;
        }
        let after = tail.iter().position(|word| word == "--");
        return Some(after.map_or_else(Vec::new, |at| tail[at + 1..].to_vec()));
    }
    let (program, arguments) = rest.split_first()?;
    (program == "relish" || program.ends_with("/relish")).then(|| arguments.to_vec())
}

/// Every `relish` invocation in a shell block, as the arguments it passes.
fn relish_invocations(snippet: &Snippet) -> Vec<Vec<String>> {
    logical_lines(snippet)
        .iter()
        .flat_map(|line| simple_commands(&fill_placeholders(line)))
        .filter_map(|words| relish_arguments(&words))
        .collect()
}

/// What reads a TOML block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TomlKind {
    /// `relish apply`: `Config::parse` then `validate()`.
    Workload,
    /// `node.toml`: `NodeConfig::parse`.
    Node,
    /// `relish fault scenario`: `[[step]]` entries.
    Scenario,
    /// `relish batch submit`: `[[cohort]]` entries.
    Manifest,
}

const WORKLOAD_TABLES: &[&str] = &["app", "job", "namespace", "permission", "build"];

/// Split a TOML document by the type that reads each top-level key. Anything
/// that isn't a workload table, a scenario or a manifest is node config, so a
/// misspelt table lands in `NodeConfig` and fails there by name.
fn classify(document: &toml::Table) -> BTreeMap<TomlKind, toml::Table> {
    let whole = if document.contains_key("step") {
        Some(TomlKind::Scenario)
    } else if document.contains_key("cohort") {
        Some(TomlKind::Manifest)
    } else {
        None
    };
    let mut parts: BTreeMap<TomlKind, toml::Table> = BTreeMap::new();
    for (key, value) in document {
        let kind = whole.unwrap_or(if WORKLOAD_TABLES.contains(&key.as_str()) {
            TomlKind::Workload
        } else {
            TomlKind::Node
        });
        parts
            .entry(kind)
            .or_default()
            .insert(key.clone(), value.clone());
    }
    parts
}

// --- Checks ------------------------------------------------------------------

/// Run the real `relish` with `RELISH_PARSE_ONLY=1`: it parses the arguments
/// and exits without doing anything.
fn parses_with_real_cli(arguments: &[String]) -> Result<(), String> {
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(arguments)
        .env("RELISH_PARSE_ONLY", "1")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env_remove("RELIABURGER_ENDPOINT")
        .output()
        .unwrap();
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn check_toml_part(kind: TomlKind, source: &str) -> Result<(), String> {
    use reliaburger::config::{Config, NodeConfig};
    match kind {
        TomlKind::Workload => {
            let config = Config::parse(source).map_err(|error| error.to_string())?;
            config.validate().map_err(|error| error.to_string())
        }
        TomlKind::Node => {
            // The checks `bun` runs on its config at startup, in its order.
            let text = |error: reliaburger::config::ConfigError| error.to_string();
            let config = NodeConfig::parse(source).map_err(text)?;
            config.reconstruction.validate().map_err(text)?;
            config.cluster.backup.validate()?;
            config.cluster.validate().map_err(text)?;
            config.alerts.validate().map_err(text)?;
            config.ingress.validate().map_err(text)?;
            config.validate().map_err(text)
        }
        TomlKind::Scenario => {
            let scenario: reliaburger::smoker::types::ScriptedScenario =
                toml::from_str(source).map_err(|error| error.to_string())?;
            reliaburger::smoker::scenario::build_timeline(&scenario, 1.0)
                .map(drop)
                .map_err(|error| error.to_string())
        }
        TomlKind::Manifest => {
            toml::from_str::<reliaburger::bun::task_array_api::TaskManifestRequest>(source)
                .map(drop)
                .map_err(|error| error.to_string())
        }
    }
}

/// Check a TOML block, keeping only the parts `wanted` selects. A block
/// that is one kind is checked from its own text, so errors carry its line
/// numbers; a mixed block is split and each part re-serialised.
fn check_toml(snippet: &Snippet, wanted: &[TomlKind]) -> Vec<String> {
    let document: toml::Table = match toml::from_str(&snippet.body) {
        Ok(document) => document,
        // Broken TOML belongs to whichever check runs first; report it once.
        Err(error) if wanted.contains(&TomlKind::Workload) => {
            return vec![format!("not valid TOML: {error}")];
        }
        Err(_) => return Vec::new(),
    };
    let parts = classify(&document);
    let single = parts.len() == 1;
    parts
        .into_iter()
        .filter(|(kind, _)| wanted.contains(kind))
        .filter_map(|(kind, part)| {
            let source = if single {
                snippet.body.clone()
            } else {
                toml::to_string(&part).unwrap()
            };
            check_toml_part(kind, &source)
                .err()
                .map(|error| format!("{kind:?} config: {error}"))
        })
        .collect()
}

/// The checked snippets of `files`. Marker problems are the marker test's job.
fn snippets_of(files: &[String]) -> Vec<Snippet> {
    files
        .iter()
        .flat_map(|file| extract(file, &read(file)).0)
        .collect()
}

/// Apply a block's marker to the failures its check found.
///
/// - No marker: the block must pass.
/// - `fragment` and `planned`: never reached; the caller skips them.
/// - `pending #N`: the block must still fail, so the marker lapses when
///   issue N lands.
fn judge(snippet: &Snippet, failures: Vec<String>, report: &mut Vec<String>) {
    match (&snippet.marker, failures.is_empty()) {
        (None, true) | (Some(Marker::Pending(_)), false) => {}
        (Some(Marker::Pending(issue)), true) => report.push(format!(
            "{}: marked `pending #{issue}` but it passes now; remove the marker",
            snippet.location()
        )),
        (_, false) => report.push(format!(
            "{}:\n  {}",
            snippet.location(),
            failures.join("\n  ")
        )),
        (Some(_), true) => {}
    }
}

fn skipped(snippet: &Snippet) -> bool {
    matches!(
        snippet.marker,
        Some(Marker::Fragment | Marker::Planned(_) | Marker::Unknown(_))
    )
}

/// Check every shell block in `files`. A `pending` block passes only while
/// every `relish` command in it still fails: split working lines out of it.
fn check_shell_snippets(files: &[String]) -> (usize, Vec<String>) {
    let mut checked = 0;
    let mut report = Vec::new();
    for snippet in snippets_of(files) {
        if snippet.language == Language::Toml || skipped(&snippet) {
            continue;
        }
        let mut failures = Vec::new();
        let mut passing = Vec::new();
        for arguments in relish_invocations(&snippet) {
            checked += 1;
            let command = format!("relish {}", arguments.join(" "));
            match parses_with_real_cli(&arguments) {
                Ok(()) => passing.push(command),
                Err(error) => failures.push(format!("`{command}`: {error}")),
            }
        }
        if let (Some(Marker::Pending(issue)), false, false) =
            (&snippet.marker, failures.is_empty(), passing.is_empty())
        {
            report.push(format!(
                "{}: marked `pending #{issue}`, but these parse today; move them to their \
                 own block: {passing:?}",
                snippet.location()
            ));
            continue;
        }
        judge(&snippet, failures, &mut report);
    }
    (checked, report)
}

fn check_toml_snippets(kinds: &[TomlKind]) -> (usize, Vec<String>) {
    let mut checked = 0;
    let mut report = Vec::new();
    for snippet in snippets_of(&all_files()) {
        if snippet.language != Language::Toml || skipped(&snippet) {
            continue;
        }
        let relevant = toml::from_str::<toml::Table>(&snippet.body)
            .map(|document| classify(&document).keys().any(|kind| kinds.contains(kind)))
            .unwrap_or(kinds.contains(&TomlKind::Workload));
        if !relevant {
            continue;
        }
        checked += 1;
        let failures = check_toml(&snippet, kinds);
        judge(&snippet, failures, &mut report);
    }
    (checked, report)
}

#[test]
fn every_relish_command_in_the_manual_parses_with_the_real_cli() {
    let (checked, failures) = check_shell_snippets(&manual_files());
    assert!(
        checked > 100,
        "found only {checked} commands; did the extractor break?"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_relish_command_in_the_readmes_and_whitepaper_parses() {
    let files: Vec<String> = OTHER_FILES.iter().map(|file| file.to_string()).collect();
    let (checked, failures) = check_shell_snippets(&files);
    assert!(
        checked > 30,
        "found only {checked} commands; did the extractor break?"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Workload configs, plus the two other files `relish` reads: chaos
/// scenarios and batch manifests.
#[test]
fn every_workload_toml_snippet_parses_and_validates() {
    let (checked, failures) =
        check_toml_snippets(&[TomlKind::Workload, TomlKind::Scenario, TomlKind::Manifest]);
    assert!(checked > 30, "found only {checked} workload blocks");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_node_toml_snippet_parses() {
    let (checked, failures) = check_toml_snippets(&[TomlKind::Node]);
    assert!(checked > 20, "found only {checked} node blocks");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn snippet_opt_outs_are_explicit_and_pending_issues_are_open() {
    let mut failures = Vec::new();
    let mut listed = Vec::new();
    for file in all_files() {
        let (snippets, orphans) = extract(&file, &read(&file));
        for (line, marker) in orphans {
            failures.push(format!(
                "{file}:{line}: {marker:?} isn't on the line before a sh, bash, shell, \
                 console or toml fence"
            ));
        }
        for snippet in snippets {
            let Some(marker) = &snippet.marker else {
                continue;
            };
            match marker {
                Marker::Unknown(text) => failures.push(format!(
                    "{}: unknown marker `snippet: {text}`; use fragment, planned #N or pending #N",
                    snippet.location()
                )),
                Marker::Planned(issue) | Marker::Pending(issue)
                    if !OPEN_ISSUES.iter().any(|(open, _)| open == issue) =>
                {
                    failures.push(format!(
                        "{}: {marker:?} names #{issue}, which isn't in OPEN_ISSUES; if it \
                         closed, make the snippet real and drop the marker",
                        snippet.location()
                    ))
                }
                _ => {}
            }
            listed.push(format!("{}: {marker:?}", snippet.location()));
        }
    }
    // Shown with `--no-capture`, and with any failure: every opt-out, for review.
    eprintln!("{} snippet markers:\n{}", listed.len(), listed.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- Inline commands in prose ------------------------------------------------

/// One inline `relish …` code span outside a fence.
#[derive(Debug, Clone)]
struct InlineSpan {
    location: String,
    text: String,
    /// The span is followed by ` *(planned)*`.
    labelled_planned: bool,
}

fn inline_spans(file: &str, text: &str) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut in_fence = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let pieces: Vec<&str> = line.split('`').collect();
        for (at, span) in pieces.iter().enumerate().skip(1).step_by(2) {
            if !span.starts_with("relish ") || at + 1 >= pieces.len() {
                continue;
            }
            spans.push(InlineSpan {
                location: format!("{file}:{}", index + 1),
                text: span.to_string(),
                labelled_planned: pieces[at + 1].starts_with(" *(planned)*"),
            });
        }
    }
    spans
}

/// `<APP>`, `ID`, `a|b`, `[ARGS]...`, `$DATE`, `…`: stands for a value.
fn is_placeholder(word: &str) -> bool {
    let all_caps = word.chars().any(|c| c.is_ascii_uppercase())
        && !word.chars().any(|c| c.is_ascii_lowercase());
    word.contains(['<', '[', '|', '$', '…']) || word.contains("...") || all_caps
}

fn is_subcommand_word(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_lowercase())
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `relish <path> --help`, cached: the help text, or why the path isn't a
/// command.
fn help_text(
    path: &[String],
    cache: &mut BTreeMap<Vec<String>, Result<String, String>>,
) -> Result<String, String> {
    if let Some(known) = cache.get(path) {
        return known.clone();
    }
    let mut arguments = path.to_vec();
    // `relish help --help` isn't a command, but `relish help` is.
    if path != ["help"] {
        arguments.push("--help".into());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(&arguments)
        .env("RELISH_PARSE_ONLY", "1")
        .output()
        .unwrap();
    let result = if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        Err(error.lines().next().unwrap_or_default().to_string())
    };
    cache.insert(path.to_vec(), result.clone());
    result
}

/// Whether `flag` is in a help text's option list and, if so, whether it
/// takes a value.
fn option_takes_value(help: &str, flag: &str) -> Option<bool> {
    help.lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with('-'))
        .map(|line| line.split("  ").next().unwrap_or_default())
        .find(|options| options.split([',', ' ']).any(|name| name == flag))
        .map(|options| options.contains('<'))
}

/// Check an inline span names a real subcommand with real flags. Leading
/// subcommand-shaped words are the path; a flag's value is skipped when its
/// help says it takes one.
fn check_inline_span(
    span: &str,
    cache: &mut BTreeMap<Vec<String>, Result<String, String>>,
) -> Result<(), String> {
    let words: Vec<String> = span
        .split_whitespace()
        .skip(1)
        .map(|word| word.trim_matches(['"', '\'']).to_string())
        .collect();
    let mut path: Vec<String> = Vec::new();
    let mut path_open = true;
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        index += 1;
        if word == "--" {
            // What follows goes to the workload, not to `relish`.
            break;
        } else if is_placeholder(word) {
            path_open = false;
        } else if word.starts_with('-') {
            let flag = word.split('=').next().unwrap_or_default();
            let help = help_text(&path, cache)?;
            let takes_value = option_takes_value(&help, flag)
                .ok_or_else(|| format!("`relish {}` has no {flag} option", path.join(" ")))?;
            let value_follows = words.get(index).is_some_and(|next| !next.starts_with('-'));
            if takes_value && !word.contains('=') && value_follows {
                index += 1;
            }
        } else if path_open && is_subcommand_word(word) {
            path.push(word.clone());
        } else {
            path_open = false;
        }
    }
    help_text(&path, cache).map(drop)
}

#[test]
fn every_inline_relish_command_names_a_real_command_and_flags() {
    let mut cache = BTreeMap::new();
    let mut failures = Vec::new();
    let mut allowed = Vec::new();
    let mut checked = 0;
    let mut pending_seen: Vec<&str> = Vec::new();
    for file in all_files() {
        for span in inline_spans(&file, &read(&file)) {
            checked += 1;
            let pending = INLINE_PENDING.iter().find(|(text, _)| *text == span.text);
            let result = check_inline_span(&span.text, &mut cache);
            let excuse = match (pending, span.labelled_planned) {
                (Some((_, issue)), _) => Some(format!("INLINE_PENDING #{issue}")),
                (None, true) => Some("*(planned)*".to_string()),
                (None, false) => None,
            };
            if let Some((text, _)) = pending {
                pending_seen.push(text);
            }
            match (result, excuse) {
                (Ok(()), None) => {}
                (Err(error), None) => {
                    failures.push(format!("{}: `{}`: {error}", span.location, span.text))
                }
                (Err(_), Some(excuse)) => {
                    allowed.push(format!("{}: `{}` ({excuse})", span.location, span.text))
                }
                (Ok(()), Some(excuse)) => failures.push(format!(
                    "{}: `{}` works now; remove its {excuse}",
                    span.location, span.text
                )),
            }
        }
    }
    for (text, issue) in INLINE_PENDING {
        if !OPEN_ISSUES.iter().any(|(open, _)| open == issue) {
            failures.push(format!(
                "INLINE_PENDING `{text}` names #{issue}, not in OPEN_ISSUES"
            ));
        }
        if !pending_seen.contains(text) {
            failures.push(format!(
                "INLINE_PENDING lists `{text}`, but no doc mentions it"
            ));
        }
    }
    eprintln!(
        "{} inline commands not yet real:\n{}",
        allowed.len(),
        allowed.join("\n")
    );
    assert!(checked > 300, "found only {checked} inline commands");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- Field references --------------------------------------------------------

/// Fields the config accepts that a user never writes: the leader fills them.
const INTERNAL_FIELDS: &[&str] = &["ordinals"];

/// The fields a `deny_unknown_fields` table accepts, read from the error serde
/// gives for one it doesn't know, so the list can't drift from the struct.
fn accepted_fields<T: serde::de::DeserializeOwned>() -> Vec<String> {
    let error = match toml::from_str::<T>("no_such_field = 1") {
        Ok(_) => panic!("the table accepted an unknown field"),
        Err(error) => error.to_string(),
    };
    let Some((_, list)) = error.split_once("expected one of") else {
        panic!("serde's unknown-field error changed shape: {error}");
    };
    list.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .filter(|field| !INTERNAL_FIELDS.contains(&field.as_str()))
        .collect()
}

/// The text of a `## heading` section, up to the next `## `.
fn manual_section(text: &str, heading: &str) -> String {
    let start = text
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("no `{heading}` section"));
    let rest = &text[start + heading.len() + 2..];
    rest[..rest.find("\n## ").unwrap_or(rest.len())].to_string()
}

#[test]
fn manual_reference_lists_every_app_and_job_field() {
    use reliaburger::config::app::{AppSpec, HealthSpec};
    use reliaburger::config::job::JobSpec;

    let reference = read("docs/manual/13_reference.md");
    let tables = [
        ("## App fields", accepted_fields::<AppSpec>()),
        ("## App fields", accepted_fields::<HealthSpec>()),
        ("## Job fields", accepted_fields::<JobSpec>()),
    ];
    let mut missing = Vec::new();
    for (heading, fields) in tables {
        assert!(fields.len() > 5, "found only {fields:?} under {heading}");
        let section = manual_section(&reference, heading);
        for field in fields {
            let named = section.contains(&format!("`{field}`"))
                || section.contains(&format!("`[[{field}]]`"));
            if !named {
                missing.push(format!("{heading}: `{field}`"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "docs/manual/13_reference.md doesn't describe:\n{}",
        missing.join("\n")
    );
}

// --- The CLI design doc ------------------------------------------------------

/// `docs/design/cli-relish.md` describes what `relish` does. Every inline
/// `relish …` span outside its "Planned commands" section must name a real
/// subcommand with real flags; a planned one there or elsewhere is labelled
/// `*(planned)*`, and must still fail, so the label lapses when it lands.
#[test]
fn cli_design_doc_names_no_missing_commands() {
    let file = "docs/design/cli-relish.md";
    let text = read(file);
    let planned_start = text
        .find("\n### Planned commands")
        .unwrap_or_else(|| panic!("{file} has no Planned commands section"));
    let planned_end = text[planned_start + 1..]
        .find("\n#")
        .map_or(text.len(), |end| planned_start + 1 + end);
    let planned_lines =
        text[..planned_start].lines().count() + 1..=text[..planned_end].lines().count();

    let mut cache = BTreeMap::new();
    let mut failures = Vec::new();
    let mut checked = 0;
    for span in inline_spans(file, &text) {
        let line: usize = span
            .location
            .rsplit(':')
            .next()
            .and_then(|number| number.parse().ok())
            .unwrap_or_default();
        if planned_lines.contains(&line) {
            continue;
        }
        checked += 1;
        let result = check_inline_span(&span.text, &mut cache);
        match (result, span.labelled_planned) {
            (Ok(()), false) | (Err(_), true) => {}
            (Err(error), false) => {
                failures.push(format!("{}: `{}`: {error}", span.location, span.text))
            }
            (Ok(()), true) => failures.push(format!(
                "{}: `{}` works now; remove its *(planned)*",
                span.location, span.text
            )),
        }
    }
    assert!(checked > 50, "found only {checked} inline commands");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- The extractor itself ----------------------------------------------------

fn shell(body: &str) -> Snippet {
    Snippet {
        file: "test.md".into(),
        line: 1,
        language: Language::Shell,
        marker: None,
        body: body.into(),
    }
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

#[test]
fn extracts_commands_across_line_continuations() {
    let snippet = shell(
        "relish apply app.toml \\\n    --dry-run\nexport RELIABURGER_TOKEN=\"$(relish --ca-cert ca.crt \\\n  token create --name ci --role deployer)\"\n",
    );
    assert_eq!(
        relish_invocations(&snippet),
        vec![
            strings(&["apply", "app.toml", "--dry-run"]),
            strings(&[
                "--ca-cert",
                "ca.crt",
                "token",
                "create",
                "--name",
                "ci",
                "--role",
                "deployer"
            ]),
        ]
    );
}

#[test]
fn command_substitution_is_its_own_command_and_a_value_outside() {
    let snippet =
        shell("relish secret encrypt --pubkey \"$(relish secret pubkey)\" 'the plaintext'\n");
    assert_eq!(
        relish_invocations(&snippet),
        vec![
            strings(&["secret", "pubkey"]),
            strings(&["secret", "encrypt", "--pubkey", "1", "the plaintext"]),
        ]
    );
}

#[test]
fn strips_prompts_and_comments() {
    let snippet = shell(
        "$ relish status          # every app\n# relish not-a-command\nsudo target/debug/relish init cluster --cluster-name 'prod one'\ncargo run --bin relish -- logs <APP> --tail 20 2>&1 | less\ncargo run --bin relish   # the TUI\nlimactl shell vm-1 relish nodes > nodes.txt\ncargo build --bin relish\n",
    );
    assert_eq!(
        relish_invocations(&snippet),
        vec![
            strings(&["status"]),
            strings(&["init", "cluster", "--cluster-name", "prod one"]),
            strings(&["logs", "1", "--tail", "20"]),
            Vec::new(),
            strings(&["nodes"]),
        ]
    );

    let console = Snippet {
        language: Language::Console,
        ..shell("$ relish nodes\nrelish output that isn't a command\n")
    };
    assert_eq!(relish_invocations(&console), vec![strings(&["nodes"])]);
}

#[test]
fn inline_spans_check_the_subcommand_and_its_flags_only() {
    let mut cache = BTreeMap::new();
    let mut check = |span: &str| check_inline_span(span, &mut cache);
    assert!(check("relish token create --name ci --role deployer").is_ok());
    assert!(check("relish --output json batch-status ID").is_ok());
    assert!(check("relish run --batch NAME --image IMAGE -- COMMAND --anything").is_ok());
    assert!(
        check("relish apply").is_ok(),
        "a missing argument is fine in prose"
    );
    assert!(check("relish help COMMAND").is_ok());
    assert!(check("relish events --severity warning").is_ok());
    assert!(check("relish -o json token list").is_ok());
    assert!(check("relish drain").is_err());
    assert!(check("relish exec --debug").is_err());

    let spans = inline_spans(
        "test.md",
        "Run `relish status` or `relish plan <path>` *(planned)*.\n```sh\n`relish no`\n```\n",
    );
    let found: Vec<(&str, bool)> = spans
        .iter()
        .map(|span| (span.text.as_str(), span.labelled_planned))
        .collect();
    assert_eq!(
        found,
        vec![("relish status", false), ("relish plan <path>", true)]
    );
}

#[test]
fn classifies_node_and_workload_tables() {
    let kinds = |source: &str| -> Vec<TomlKind> {
        classify(&toml::from_str(source).unwrap())
            .into_keys()
            .collect()
    };
    assert_eq!(
        kinds("[app.web]\nimage = 'nginx'"),
        vec![TomlKind::Workload]
    );
    assert_eq!(
        kinds("[build.api]\n[permission.ci]"),
        vec![TomlKind::Workload]
    );
    assert_eq!(kinds("[dns]\n[ebpf]"), vec![TomlKind::Node]);
    assert_eq!(
        kinds("[ingress]\nenabled = true\n[app.web]\nimage = 'nginx'"),
        vec![TomlKind::Workload, TomlKind::Node]
    );
    // Misspelt tables go to the node parser, which rejects them by name.
    assert_eq!(kinds("[apps.web]"), vec![TomlKind::Node]);
    assert_eq!(
        kinds("name = 's'\n[[step]]\nfault = 'kill'"),
        vec![TomlKind::Scenario]
    );
    assert_eq!(
        kinds("name = 'm'\nnamespace = 'default'\n[[cohort]]\nname = 'a'"),
        vec![TomlKind::Manifest]
    );
}

#[test]
fn ignores_blocks_after_a_fragment_marker() {
    let text = "Intro.\n\n<!-- snippet: fragment -->\n```toml\n[app.web.env]\n```\n\n\
                <!-- snippet: pending #656 -->\n\n```sh\nrelish drain\n```\n\n\
                ```toml\n[app.web]\n```\n\n<!-- snippet: planned #711 -->\nProse.\n\n\
                <!-- snippet: framgent -->\n```sh\nrelish status\n```\n";
    let (snippets, orphans) = extract("test.md", text);
    let markers: Vec<_> = snippets.iter().map(|s| s.marker.clone()).collect();
    assert_eq!(
        markers,
        vec![
            Some(Marker::Fragment),
            Some(Marker::Pending(656)),
            None,
            Some(Marker::Unknown("framgent".into())),
        ]
    );
    assert!(skipped(&snippets[0]));
    assert!(!skipped(&snippets[1]), "pending blocks are still checked");
    assert!(
        skipped(&snippets[3]),
        "an unknown marker fails the marker test"
    );
    assert_eq!(orphans, vec![(18, Marker::Planned(711))]);
}
