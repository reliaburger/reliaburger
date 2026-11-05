//! `relish agent source show`: numbered lines of one file, and what Rust
//! item they're inside.

use schemars::JsonSchema;
use serde::Serialize;

use super::{Answer, Bundle, normalise_path, not_found, quote};
use crate::relish::agent::envelope::{AgentError, ErrorCode};

/// The most lines one `show` prints.
pub const MAX_SHOW_LINES: usize = 400;

/// `data` for `relish agent source show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ShowData {
    /// Repository-relative path.
    pub path: String,
    /// The first line shown, counting from 1.
    pub from: usize,
    /// The last line shown.
    pub to: usize,
    /// How many lines the file has.
    pub total_lines: usize,
    /// The Rust items the first line is inside, outermost first (an `impl`,
    /// then a `fn`). Found by scanning upwards for item headers with less
    /// indentation, not by parsing, so treat it as a strong hint. Empty for
    /// a file that isn't Rust, or a range that starts at the top level.
    pub enclosing: Vec<EnclosingItem>,
    /// GitHub link to these lines at the build's commit, when the build is
    /// clean.
    pub url: Option<String>,
    /// The lines, numbered.
    pub lines: Vec<ShownLine>,
}

/// One numbered line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ShownLine {
    /// Line number, counting from 1.
    pub number: usize,
    /// The line, without its newline.
    pub text: String,
}

/// A Rust item a line is inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct EnclosingItem {
    /// What kind of item.
    pub kind: ItemKind,
    /// A `fn`'s, `mod`'s or `trait`'s name; an `impl`'s header, such as
    /// `impl<T> Envelope<T>`.
    pub name: String,
    /// The line its header starts on.
    pub line: usize,
}

/// The kinds of item the upward scan recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A function or method.
    Fn,
    /// An `impl` block.
    Impl,
    /// A trait definition.
    Trait,
    /// An inline module, such as `mod tests`.
    Mod,
}

/// A `show` target: a path and an optional line range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target<'a> {
    path: &'a str,
    from: Option<usize>,
    to: Option<usize>,
}

/// Split `path:from-to` (or `path:line`, or just `path`). A suffix that
/// isn't a line range is part of the path.
fn parse_target(target: &str) -> Result<Target<'_>, AgentError> {
    let Some((path, range)) = target.rsplit_once(':') else {
        return Ok(Target {
            path: normalise_path(target),
            from: None,
            to: None,
        });
    };
    let number = |text: &str| -> Result<usize, AgentError> {
        match text.parse::<usize>() {
            Ok(line) if line > 0 => Ok(line),
            _ => Err(AgentError::new(
                ErrorCode::Usage,
                format!("{target:?}: lines count from 1, as path:from-to"),
            )),
        }
    };
    let looks_like_range = !range.is_empty()
        && range
            .chars()
            .all(|character| character.is_ascii_digit() || character == '-');
    if !looks_like_range {
        return Ok(Target {
            path: normalise_path(target),
            from: None,
            to: None,
        });
    }
    let (from, to) = match range.split_once('-') {
        Some((from, to)) => (number(from)?, Some(number(to)?)),
        None => (number(range)?, None),
    };
    if to.is_some_and(|to| to < from) {
        return Err(AgentError::new(
            ErrorCode::Usage,
            format!("{target:?}: the range ends before it starts"),
        ));
    }
    Ok(Target {
        path: normalise_path(path),
        from: Some(from),
        to,
    })
}

pub(super) fn show(bundle: &Bundle, target: &str) -> Result<Answer<ShowData>, AgentError> {
    let target = parse_target(target)?;
    let file = bundle
        .file(target.path)
        .ok_or_else(|| not_found(bundle, target.path))?;
    let text = String::from_utf8_lossy(&file.contents);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let from = target.from.unwrap_or(1);
    if from > total.max(1) {
        return Err(AgentError::new(
            ErrorCode::Usage,
            format!("{} has {total} lines; line {from} isn't one", file.path),
        ));
    }
    let asked_to = target.to.unwrap_or(from + MAX_SHOW_LINES - 1);
    let to = asked_to.min(from + MAX_SHOW_LINES - 1).min(total);
    let shown = lines
        .get(from.saturating_sub(1)..to)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(offset, line)| ShownLine {
            number: from + offset,
            text: line.to_string(),
        })
        .collect();
    let enclosing = if file.path.ends_with(".rs") {
        enclosing_items(&lines, from)
    } else {
        Vec::new()
    };
    let anchor = if from == to {
        format!("L{from}")
    } else {
        format!("L{from}-L{to}")
    };
    let mut answer = Answer::new(ShowData {
        path: file.path.clone(),
        from,
        to,
        total_lines: total,
        enclosing,
        url: bundle.permalink(&file.path, &anchor),
        lines: shown,
    });
    if to < asked_to.min(total) {
        answer.warnings.push(format!(
            "showing {MAX_SHOW_LINES} lines, the most one show prints"
        ));
    }
    if to < total {
        let next_to = (to + MAX_SHOW_LINES).min(total);
        answer = answer.with_next(
            format!(
                "relish agent source show {}",
                quote(&format!("{}:{}-{next_to}", file.path, to + 1))
            ),
            "the lines after these",
            true,
        );
    }
    Ok(answer)
}

/// The items line `from` (counting from 1) of a Rust file is inside,
/// outermost first.
///
/// It walks upwards, remembering the shallowest indentation seen so far. A
/// line indented less than that is the start of something that contains
/// everything below it: an item header if it is one, otherwise (a closing
/// brace, a `where`, a signature's `) -> T {`) a line that lowers the bar.
/// A line starting with `)`, `>` or `where` belongs to a multi-line header
/// whose first line has the same indentation, so it lowers the bar only to
/// just above that.
pub fn enclosing_items(lines: &[&str], from: usize) -> Vec<EnclosingItem> {
    let start = from.saturating_sub(1).min(lines.len());
    let Some(mut limit) = lines[start..]
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| indentation(line))
    else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for index in (0..start).rev() {
        let line = lines[index];
        if line.trim().is_empty() || limit == 0 {
            continue;
        }
        let indent = indentation(line);
        if indent >= limit {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some((kind, name)) = item_header(trimmed) {
            items.push(EnclosingItem {
                kind,
                name,
                line: index + 1,
            });
            limit = indent;
        } else if trimmed.starts_with(')')
            || trimmed.starts_with('>')
            || trimmed.starts_with("where")
        {
            limit = indent + 1;
        } else if !trimmed.starts_with("//") && !trimmed.starts_with('#') {
            limit = indent;
        }
    }
    items.reverse();
    items
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The kind and name of the item `line` (already trimmed) starts, if it
/// starts one.
fn item_header(line: &str) -> Option<(ItemKind, String)> {
    let mut rest = line;
    if let Some(after) = rest.strip_prefix("pub") {
        rest = match after.strip_prefix('(') {
            Some(scope) => scope.split_once(')')?.1,
            None if after.starts_with(' ') => after,
            None => return None,
        }
        .trim_start();
    }
    for qualifier in ["default ", "const ", "async ", "unsafe ", "extern \"C\" "] {
        if let Some(after) = rest.strip_prefix(qualifier) {
            rest = after.trim_start();
        }
    }
    let header = rest.trim_end().trim_end_matches('{').trim_end();
    if let Some(after) = rest.strip_prefix("fn ") {
        return Some((ItemKind::Fn, identifier(after)?));
    }
    if rest.starts_with("impl ") || rest.starts_with("impl<") {
        return Some((ItemKind::Impl, header.to_string()));
    }
    if let Some(after) = rest.strip_prefix("trait ") {
        return Some((ItemKind::Trait, identifier(after)?));
    }
    if let Some(after) = rest.strip_prefix("mod ") {
        // `mod name;` declares a module in another file; it contains nothing.
        let name = identifier(after)?;
        return (!after.trim_end().ends_with(';')).then_some((ItemKind::Mod, name));
    }
    None
}

fn identifier(text: &str) -> Option<String> {
    let name: String = text
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::super::SourceCommand;
    use super::super::fixture::*;
    use super::super::nodes::ClusterSource;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;

    fn run(target: &str) -> (serde_json::Value, u8) {
        answer_json(
            &bundle(),
            &SourceCommand::Show {
                target: target.to_string(),
            },
            &ClusterSource::Offline,
            DEFAULT_MAX_BYTES,
        )
    }

    fn names(from: usize) -> Vec<String> {
        let lines: Vec<&str> = CLIENT_RS.lines().collect();
        enclosing_items(&lines, from)
            .into_iter()
            .map(|item| format!("{:?} {} @{}", item.kind, item.name, item.line))
            .collect()
    }

    #[test]
    fn snapshot_of_a_range_inside_a_method() {
        let (value, exit) = run("src/client.rs:20-23");
        assert_eq!(exit, 0);
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn a_line_in_a_method_is_inside_the_method_and_its_impl() {
        // Line 12 is `timeout: Duration::from_secs(10),` in `fn new`.
        assert_eq!(names(12), ["Impl impl Client @9", "Fn new @10"]);
    }

    #[test]
    fn a_multi_line_signature_still_names_its_function() {
        // Line 21 is in `fetch`, whose header spans lines 16 to 19.
        assert_eq!(names(21), ["Impl impl Client @9", "Fn fetch @16"]);
    }

    #[test]
    fn a_sibling_function_above_is_not_the_enclosing_one() {
        // Line 28 is in `classify_error`, after `impl Client` closed.
        assert_eq!(names(28), ["Fn classify_error @26"]);
    }

    #[test]
    fn a_test_names_its_module_and_function() {
        // Line 40 is the assertion in the test.
        assert_eq!(
            names(40),
            ["Mod tests @35", "Fn a_timeout_is_a_timeout @39"]
        );
    }

    #[test]
    fn the_top_level_is_inside_nothing() {
        assert_eq!(names(1), Vec::<String>::new());
        assert_eq!(names(26), Vec::<String>::new());
    }

    #[test]
    fn item_headers_with_visibility_and_qualifiers_are_recognised() {
        let cases = [
            ("pub fn new() -> Self {", Some((ItemKind::Fn, "new"))),
            ("pub(crate) async fn fetch(", Some((ItemKind::Fn, "fetch"))),
            (
                "pub(in crate::x) const unsafe fn raw()",
                Some((ItemKind::Fn, "raw")),
            ),
            (
                "impl<T> Envelope<T> {",
                Some((ItemKind::Impl, "impl<T> Envelope<T>")),
            ),
            ("pub trait Grill {", Some((ItemKind::Trait, "Grill"))),
            ("mod tests {", Some((ItemKind::Mod, "tests"))),
            ("mod nodes;", None),
            ("public_fn();", None),
            ("let fn_name = 1;", None),
        ];
        for (line, expected) in cases {
            let got = item_header(line);
            let expected = expected.map(|(kind, name)| (kind, name.to_string()));
            assert_eq!(got, expected, "{line}");
        }
    }

    #[test]
    fn a_whole_file_is_shown_up_to_the_limit() {
        let (value, _) = run("src/lib.rs");
        assert_eq!(value["data"]["from"], 1);
        assert_eq!(value["data"]["to"], 1);
        assert_eq!(value["data"]["lines"][0]["text"], "pub mod client;");
        assert_eq!(value["next"], serde_json::json!([]));
    }

    #[test]
    fn a_long_range_stops_at_four_hundred_lines_and_offers_the_rest() {
        let long: String = (1..=1000).map(|line| format!("// line {line}\n")).collect();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("long.rs"), long).unwrap();
        let files =
            crate::relish::source_bundle::pack::list_files(dir.path(), &["long.rs"]).unwrap();
        let tar = crate::relish::source_bundle::pack::write_tar(dir.path(), &files).unwrap();
        let bundle = Bundle::from_tar(tar, build(Some(false))).unwrap();
        let answer = show(&bundle, "long.rs:10-900").unwrap();
        assert_eq!(answer.data.from, 10);
        assert_eq!(answer.data.to, 409);
        assert_eq!(answer.data.lines.len(), MAX_SHOW_LINES);
        assert_eq!(
            answer.next[0].run,
            "relish agent source show long.rs:410-809"
        );
        assert_eq!(answer.warnings.len(), 1);
    }

    #[test]
    fn a_missing_path_is_not_found_with_close_matches() {
        let (value, exit) = run("src/relish/client.rs:10-20");
        assert_eq!(exit, 5);
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn bad_ranges_are_usage_errors() {
        for target in [
            "src/client.rs:0-3",
            "src/client.rs:9-3",
            "src/client.rs:999",
        ] {
            let (value, exit) = run(target);
            assert_eq!(exit, 2, "{target}");
            assert_eq!(value["error"]["code"], "usage", "{target}");
        }
    }

    #[test]
    fn targets_parse_with_and_without_ranges() {
        let parse = |target| parse_target(target).unwrap();
        assert_eq!(
            parse("src/a.rs:3-9"),
            Target {
                path: "src/a.rs",
                from: Some(3),
                to: Some(9)
            }
        );
        assert_eq!(
            parse("src/a.rs:7"),
            Target {
                path: "src/a.rs",
                from: Some(7),
                to: None
            }
        );
        assert_eq!(
            parse("./src/a.rs"),
            Target {
                path: "src/a.rs",
                from: None,
                to: None
            }
        );
        assert_eq!(parse("odd:name").path, "odd:name");
    }
}
