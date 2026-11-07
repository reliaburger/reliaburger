//! `relish agent source grep`: a regular expression over the bundle, in
//! memory, line by line.

use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::Serialize;

use super::show::{ItemKind, MAX_SHOW_LINES, enclosing_items};
use super::{Answer, Bundle, quote, text_of};
use crate::relish::agent::envelope::{AgentError, ErrorCode};

/// The longest line a match quotes, in bytes. Minified dashboard assets
/// have lines of hundreds of kilobytes; nobody wants all of one.
const MAX_LINE_BYTES: usize = 400;

/// How far above a match its function may start for `next` to show the
/// function from the top; further than that, it shows the lines around
/// the match instead.
const MAX_FUNCTION_REACH: usize = 200;

/// The arguments of `relish agent source grep`.
#[derive(Debug, Clone, clap::Args)]
pub struct GrepArgs {
    /// A Rust regular expression (the `regex` crate's syntax), matched
    /// against one line at a time.
    pub pattern: String,
    /// Only files whose repository-relative path matches this glob: `*` and
    /// `?` within a path segment, `**` across them. A glob without `/`
    /// matches file names, and a directory matches everything under it.
    #[arg(long)]
    pub path: Option<String>,
    /// Lines of context before and after each match.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(0..=20))]
    pub context: u32,
    /// The most matches to return; `total_matches` counts them all.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub max_matches: u32,
    /// Match regardless of case.
    #[arg(long, short = 'i')]
    pub ignore_case: bool,
}

impl GrepArgs {
    /// `pattern` with every option at its default.
    pub fn new(pattern: &str) -> Self {
        GrepArgs {
            pattern: pattern.to_string(),
            path: None,
            context: 2,
            max_matches: 50,
            ignore_case: false,
        }
    }
}

/// `data` for `relish agent source grep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GrepData {
    /// The regular expression.
    pub pattern: String,
    /// The `--path` glob, if one was given.
    pub path: Option<String>,
    /// Whether case was ignored.
    pub ignore_case: bool,
    /// Lines of context around each match.
    pub context: u32,
    /// How many text files were searched (binary files are skipped).
    pub files_searched: usize,
    /// How many lines matched in all, including any past `--max-matches`.
    pub total_matches: usize,
    /// The first matches, in path and line order. An empty list means
    /// nothing matched, which is an answer, not an error.
    pub matches: Vec<GrepMatch>,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GrepMatch {
    /// Repository-relative path, e.g. `src/relish/client.rs`.
    pub path: String,
    /// Line number, counting from 1.
    pub line: usize,
    /// The matching line (cut at 400 bytes).
    pub text: String,
    /// The lines before it, nearest last.
    pub before: Vec<String>,
    /// The lines after it.
    pub after: Vec<String>,
    /// GitHub link to the line at the build's commit, when the build is
    /// clean.
    pub url: Option<String>,
}

/// `glob` as an anchored regular expression over repository-relative paths.
fn glob_regex(glob: &str) -> Result<Regex, AgentError> {
    let glob = super::normalise_path(glob);
    let mut pattern = String::from("^");
    if !glob.contains('/') {
        // A bare file-name glob matches in any directory.
        pattern.push_str("(?:.*/)?");
    }
    let mut chars = glob.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    pattern.push_str("(?:.*/)?");
                } else {
                    pattern.push_str(".*");
                }
            }
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            other => pattern.push_str(&regex::escape(&other.to_string())),
        }
    }
    // A directory matches everything under it.
    pattern.push_str("(?:/.*)?$");
    Regex::new(&pattern)
        .map_err(|error| AgentError::new(ErrorCode::Usage, format!("bad --path glob: {error}")))
}

/// `line` cut to at most [`MAX_LINE_BYTES`], on a character boundary.
fn clip(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

pub(super) fn grep(bundle: &Bundle, args: &GrepArgs) -> Result<Answer<GrepData>, AgentError> {
    let regex = RegexBuilder::new(&args.pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .map_err(|error| {
            AgentError::new(ErrorCode::Usage, format!("bad regular expression: {error}"))
                .with_hint("the syntax is the regex crate's: https://docs.rs/regex/1/regex/#syntax")
        })?;
    let glob = args.path.as_deref().map(glob_regex).transpose()?;
    let context = args.context as usize;
    let max_matches = args.max_matches as usize;

    let mut files_searched = 0;
    let mut total_matches = 0;
    let mut matches = Vec::new();
    // The lines of the file the first match is in, for `next`.
    let mut first_file: Option<Vec<&str>> = None;
    for file in &bundle.files {
        if glob.as_ref().is_some_and(|glob| !glob.is_match(&file.path)) {
            continue;
        }
        let Some(text) = text_of(file) else {
            continue;
        };
        files_searched += 1;
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            total_matches += 1;
            if matches.len() >= max_matches {
                continue;
            }
            let before = &lines[index.saturating_sub(context)..index];
            let after = &lines[index + 1..(index + 1 + context).min(lines.len())];
            matches.push(GrepMatch {
                path: file.path.clone(),
                line: index + 1,
                text: clip(line),
                before: before.iter().map(|line| clip(line)).collect(),
                after: after.iter().map(|line| clip(line)).collect(),
                url: bundle.permalink(&file.path, &format!("L{}", index + 1)),
            });
            if first_file.is_none() {
                first_file = Some(lines.clone());
            }
        }
    }

    let mut answer = Answer::new(GrepData {
        pattern: args.pattern.clone(),
        path: args.path.clone(),
        ignore_case: args.ignore_case,
        context: args.context,
        files_searched,
        total_matches,
        matches,
    });
    if let (Some(top), Some(lines)) = (answer.data.matches.first(), first_file) {
        let (run, why) = show_step(&top.path, top.line, &lines);
        answer = answer.with_next(run, why, true);
    }
    if total_matches > answer.data.matches.len() {
        let mut narrower = format!("relish agent source grep {}", quote(&args.pattern));
        if args.ignore_case {
            narrower.push_str(" --ignore-case");
        }
        narrower.push_str(" --path 'src/**'");
        answer = answer.with_next(
            narrower,
            "more lines matched than were returned; narrow the search with --path",
            true,
        );
    }
    if total_matches == 0 && !args.ignore_case {
        answer = answer.with_next(
            format!(
                "relish agent source grep {} --ignore-case",
                quote(&args.pattern)
            ),
            "nothing matched; try again regardless of case",
            true,
        );
    }
    Ok(answer)
}

/// The `show` step for a match at `line`: its function from the top when
/// the scan finds one close enough, else the lines around it.
fn show_step(path: &str, line: usize, lines: &[&str]) -> (String, &'static str) {
    let function = if path.ends_with(".rs") {
        enclosing_items(lines, line)
            .into_iter()
            .rev()
            .find(|item| item.kind == ItemKind::Fn && line - item.line <= MAX_FUNCTION_REACH)
    } else {
        None
    };
    let (from, why) = match function {
        Some(item) => (item.line, "the function the top match is in"),
        None => (
            line.saturating_sub(20).max(1),
            "the lines around the top match",
        ),
    };
    let to = (line + 40).min(lines.len()).min(from + MAX_SHOW_LINES - 1);
    (
        format!(
            "relish agent source show {}",
            quote(&format!("{path}:{from}-{to}"))
        ),
        why,
    )
}

#[cfg(test)]
mod tests {
    use super::super::SourceCommand;
    use super::super::fixture::*;
    use super::super::nodes::ClusterSource;
    use super::*;
    use crate::relish::agent::{DEFAULT_MAX_BYTES, MIN_MAX_BYTES};

    fn run_with(bundle: &Bundle, args: GrepArgs, max_bytes: usize) -> (serde_json::Value, u8) {
        answer_json(
            bundle,
            &SourceCommand::Grep(args),
            &ClusterSource::Offline,
            max_bytes,
        )
    }

    fn run(args: GrepArgs) -> (serde_json::Value, u8) {
        run_with(&bundle(), args, DEFAULT_MAX_BYTES)
    }

    #[test]
    fn snapshot_of_a_match_with_context() {
        let mut args = GrepArgs::new("fn classify_error");
        args.context = 3;
        let (value, exit) = run(args);
        assert_eq!(exit, 0);
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn the_top_match_suggests_showing_its_function() {
        let (value, _) = run(GrepArgs::new(r"self\.timeout"));
        assert_eq!(value["data"]["matches"][0]["line"], 21);
        assert_eq!(
            value["next"][0]["run"],
            "relish agent source show src/client.rs:16-42"
        );
        assert_eq!(value["next"][0]["why"], "the function the top match is in");
        assert_eq!(value["next"][0]["read_only"], true);
    }

    #[test]
    fn no_match_is_an_answer() {
        let (value, exit) = run(GrepArgs::new("no such thing anywhere"));
        assert_eq!(exit, 0);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["matches"], serde_json::json!([]));
        assert_eq!(value["data"]["total_matches"], 0);
        assert_eq!(value["data"]["files_searched"], 5, "the PNG is skipped");
    }

    #[test]
    fn an_invalid_regex_is_a_usage_error() {
        let (value, exit) = run(GrepArgs::new("fn (unclosed"));
        assert_eq!(exit, 2);
        assert_eq!(value["error"]["code"], "usage");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("bad regular expression"),
            "{value}"
        );
    }

    #[test]
    fn permalinks_appear_only_for_a_clean_build() {
        let (clean, _) = run(GrepArgs::new("fn main"));
        assert_eq!(
            clean["data"]["matches"][0]["url"],
            format!("https://github.com/reliaburger/reliaburger/blob/{COMMIT}/src/bin/tool.rs#L1")
        );
        let dirty = Bundle::from_tar(tar(), build(Some(true))).unwrap();
        let (value, _) = run_with(&dirty, GrepArgs::new("fn main"), DEFAULT_MAX_BYTES);
        assert_eq!(value["data"]["matches"][0]["url"], serde_json::Value::Null);
        assert_eq!(value["data"]["matches"][0]["path"], "src/bin/tool.rs");
    }

    #[test]
    fn ignore_case_and_path_narrow_the_search() {
        let mut args = GrepArgs::new("the client");
        assert_eq!(run(args.clone()).0["data"]["total_matches"], 0);
        args.ignore_case = true;
        let (value, _) = run(args.clone());
        assert_eq!(value["data"]["total_matches"], 2);
        args.path = Some("*.md".to_string());
        let (value, _) = run(args);
        assert_eq!(value["data"]["total_matches"], 1);
        assert_eq!(value["data"]["files_searched"], 1);
        assert_eq!(value["data"]["matches"][0]["path"], "docs/book/01-hello.md");
    }

    #[test]
    fn globs_match_segments_directories_and_file_names() {
        let matches = |glob: &str, path: &str| glob_regex(glob).unwrap().is_match(path);
        assert!(matches("src/relish/client.rs", "src/relish/client.rs"));
        assert!(matches("src/relish", "src/relish/agent/mod.rs"));
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/relish/client.rs"));
        assert!(matches("src/**/*.rs", "src/lib.rs"));
        assert!(matches("src/**/*.rs", "src/relish/agent/mod.rs"));
        assert!(matches("*.md", "docs/book/01-hello.md"));
        assert!(matches("client.r?", "src/relish/client.rs"));
        assert!(!matches("src/rel", "src/relish/client.rs"));
        assert!(
            !matches("src/a+b.rs", "src/aab.rs"),
            "regex characters are literal"
        );
    }

    #[test]
    fn max_matches_limits_the_list_but_not_the_count() {
        let mut args = GrepArgs::new("^");
        args.max_matches = 3;
        args.context = 0;
        let (value, _) = run(args);
        assert_eq!(value["data"]["matches"].as_array().unwrap().len(), 3);
        assert!(value["data"]["total_matches"].as_u64().unwrap() > 40);
        let steps = value["next"].to_string();
        assert!(steps.contains("--path"), "{steps}");
    }

    #[test]
    fn a_big_grep_is_trimmed_to_the_budget_and_stays_valid() {
        let dir = tempfile::tempdir().unwrap();
        let big: String = (0..2000)
            .map(|line| format!("fn handler_{line}() {{ /* {} */ }}\n", "x".repeat(60)))
            .collect();
        std::fs::write(dir.path().join("big.rs"), big).unwrap();
        let files =
            crate::relish::source_bundle::pack::list_files(dir.path(), &["big.rs"]).unwrap();
        let tar = crate::relish::source_bundle::pack::write_tar(dir.path(), &files).unwrap();
        let bundle = Bundle::from_tar(tar, build(Some(false))).unwrap();
        let mut args = GrepArgs::new("fn handler_");
        args.max_matches = 1000;
        let (value, exit) = run_with(&bundle, args, MIN_MAX_BYTES * 4);
        assert_eq!(exit, 0);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["data"]["total_matches"], 2000);
        let kept = value["data"]["matches"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 1000, "kept {kept}");
        let warnings = value["warnings"].to_string();
        assert!(warnings.contains("/data/matches: kept"), "{warnings}");
        assert!(
            value["next"][0]["run"]
                .as_str()
                .unwrap()
                .contains("--max-bytes"),
            "{value}"
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "é".repeat(MAX_LINE_BYTES);
        let clipped = clip(&long);
        assert!(clipped.len() <= MAX_LINE_BYTES + "…".len());
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short"), "short");
    }
}
