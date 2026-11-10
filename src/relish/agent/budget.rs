//! Byte budgets: every `relish agent` answer fits in `--max-bytes`.
//!
//! An agent pays for every byte it reads, and a tool result over about
//! 10,000 tokens gets a warning in Claude Code, so each command's output has
//! a ceiling: 24 KiB by default, about 6,000 tokens. `relish agent guide` is
//! the exception at 80 KiB: it is the one document an agent reads whole, so
//! its default fits the whole manual chapter. `relish agent schema` gets
//! 40 KiB, so `schema status` fits whole. When an answer is
//! bigger, the budget trims `data` rather than cutting the text, so what
//! comes out is still one valid JSON document in the same shape. Then it says
//! so: `truncated` is `true`, a warning names each array or string it cut,
//! and the first `next` step is the same command with a budget big enough
//! for the whole answer.

use serde::Serialize;
use serde_json::{Value, json};

use super::envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, ExitStatus};

/// The budget when `--max-bytes` isn't given: 24 KiB, for every command
/// but `guide`.
pub const DEFAULT_MAX_BYTES: usize = 24 * 1024;

/// `relish agent guide`'s budget when `--max-bytes` isn't given: 80 KiB,
/// enough for the whole agent chapter in one call.
// From 64 KiB: raised, maintainer 8 Nov 2026. Leases and proposals (#1767,
// #1768) took the chapter past 64 KiB, and the chapter stays whole rather
// than split. 80 KiB leaves room for the `relish status` work coming next.
pub const GUIDE_MAX_BYTES: usize = 80 * 1024;

/// `relish agent schema`'s budget when `--max-bytes` isn't given: 40 KiB.
///
/// Every `schema` answer carries the envelope's schema (about 8 KiB) beside
/// the command's, and `relish status`'s is the biggest: the whole cluster
/// summary, about 21 KiB even with its fourteen sections sharing one
/// definition. An agent reads a schema once, as reference, not on every
/// question, so it gets more room the way `guide` does; every other answer
/// keeps 24 KiB.
// From 32 KiB: the 0.2.8 integration (#1764, #1767) grew the envelope
// by about 4 KiB of lease and proposal error codes and command names,
// which took `schema status` to 33,617 bytes.
pub const SCHEMA_MAX_BYTES: usize = 40 * 1024;

/// The budget `command` gets when `--max-bytes` isn't given.
pub fn default_max_bytes(command: CommandName) -> usize {
    match command {
        CommandName::Guide => GUIDE_MAX_BYTES,
        CommandName::Schema => SCHEMA_MAX_BYTES,
        _ => DEFAULT_MAX_BYTES,
    }
}

/// The smallest budget `--max-bytes` accepts. Below this even a failure
/// envelope might not fit.
pub const MIN_MAX_BYTES: usize = 1024;

/// What a shortened string ends with.
pub const STRING_CUT_MARKER: &str = "…[cut]";

tokio::task_local! {
    /// The most bytes any answer may have, when a caller holds every budget
    /// to one: `relish mcp` does (#1687). The command line has none.
    static CEILING: usize;
}

/// Run `future` with every answer it renders held to `ceiling`: a trimmed
/// answer's hint never asks for more, because asking can't get it.
///
/// A task-local rather than a parameter, so the twenty commands between
/// `relish mcp` and [`render`] needn't carry a value only one caller sets.
pub async fn within_ceiling<F: std::future::Future>(ceiling: usize, future: F) -> F::Output {
    CEILING.scope(ceiling, future).await
}

/// The ceiling the current task renders under, if any.
fn ceiling() -> Option<usize> {
    CEILING.try_with(|ceiling| *ceiling).ok()
}

/// An answer bigger than its budget: how big it is whole, and the budget
/// it was given.
///
/// Every step that asks for a bigger budget takes its number from
/// [`Overflow::rerun_budget`], whether [`render`] trims the answer or a
/// command fits it itself, as `logs` and `manual show` do. So under
/// `relish mcp` no step asks for more than the server returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow {
    /// The bytes the whole answer takes.
    pub whole: usize,
    /// The budget this answer had.
    pub max_bytes: usize,
}

impl Overflow {
    /// The budget a "get the rest" step may name: enough for the whole
    /// answer, rounded up to a KiB, or the ceiling the caller renders
    /// under when the whole is bigger. `None` once this answer already had
    /// the ceiling: asking again would get the same answer.
    pub fn rerun_budget(self) -> Option<usize> {
        match ceiling() {
            Some(ceiling) if self.whole > ceiling && self.max_bytes >= ceiling => None,
            Some(ceiling) if self.whole > ceiling => Some(ceiling),
            _ => Some(round_up_to_kib(self.whole)),
        }
    }

    /// Why the whole answer is out of reach, when the ceiling puts it
    /// there.
    pub fn out_of_reach(self) -> Option<String> {
        ceiling()
            .filter(|ceiling| self.whole > *ceiling)
            .map(|ceiling| narrow_why(self.whole, ceiling))
    }
}

/// `rerun` again with a budget of `budget` bytes: the one place a step's
/// `--max-bytes` is written, so it always comes from
/// [`Overflow::rerun_budget`].
pub fn rerun_with(rerun: &str, budget: usize) -> String {
    format!("{rerun} --max-bytes {budget}")
}

/// Why an answer bigger than the ceiling can't be had whole.
fn narrow_why(whole: usize, ceiling: usize) -> String {
    format!(
        "the whole answer is {whole} bytes, more than the {ceiling} this server returns; \
         narrow the request instead: a filter, a time range or a section"
    )
}

/// Render `envelope` as one line of JSON, newline included, in at most
/// `max_bytes` bytes.
///
/// `rerun` is the command line that produced the envelope, without
/// `--max-bytes`; a trimmed answer suggests running it again with a bigger
/// budget. A budget below [`MIN_MAX_BYTES`] counts as [`MIN_MAX_BYTES`].
pub fn render<T: Serialize>(envelope: &Envelope<T>, max_bytes: usize, rerun: &str) -> Rendered {
    let max_bytes = max_bytes.max(MIN_MAX_BYTES);
    let value = match serde_json::to_value(envelope) {
        Ok(value) => value,
        Err(error) => {
            let error = AgentError::new(
                ErrorCode::Internal,
                format!("couldn't serialise the answer: {error}"),
            );
            return failure_that_fits(envelope.command(), envelope.relish(), error, max_bytes);
        }
    };
    let whole = line_length(&value);
    if whole <= max_bytes {
        return Rendered {
            json: format!("{value}\n"),
            exit: envelope.exit_status(),
        };
    }
    let trimmed = match envelope.exit_status() {
        ExitStatus::Answered => trim_answer(value, max_bytes, whole, rerun),
        _ => trim_failure(value, max_bytes),
    };
    match trimmed {
        Some(value) => Rendered {
            json: format!("{value}\n"),
            exit: envelope.exit_status(),
        },
        None => {
            let error = AgentError::new(
                ErrorCode::Usage,
                format!("the answer is {whole} bytes and doesn't fit in {max_bytes}, even trimmed"),
            )
            .with_hint(
                Overflow { whole, max_bytes }
                    .out_of_reach()
                    .unwrap_or_else(|| {
                        format!("run it again with --max-bytes {}", round_up_to_kib(whole))
                    }),
            );
            failure_that_fits(envelope.command(), envelope.relish(), error, max_bytes)
        }
    }
}

/// A command's output, ready to print, and the exit status to end with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// One line of JSON, newline included.
    pub json: String,
    /// What the process exits with.
    pub exit: ExitStatus,
}

/// Strings no longer than this are never shortened: cutting them saves
/// little and loses what they say.
const SHORTEST_CUT_STRING: usize = 64;

/// How much of a string shortening always keeps.
const SHORTEST_KEPT_PREFIX: usize = 32;

/// Something the budget cut out of `data`, for the warning that says so.
#[derive(Debug, Clone)]
struct Cut {
    /// Where, as a JSON Pointer (RFC 6901) into `data`.
    pointer: String,
    /// The array's length or the string's byte length before any cut.
    original: usize,
    kind: CutKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutKind {
    Items,
    Bytes,
}

/// Trim an answer's `data` (then its own `next` steps and warnings) until
/// the envelope fits, or give up with `None`.
///
/// The order matters. Arrays go first, from the back, because a command puts
/// what matters most at the front (worst findings, newest events). Long
/// strings come next. Objects are never trimmed: their keys are the
/// command's schema, and an answer missing a required field is worse than
/// one with fewer items. A command whose collections are big must make them
/// arrays, not maps, so they can be trimmed.
fn trim_answer(value: Value, max_bytes: usize, whole: usize, rerun: &str) -> Option<Value> {
    let Value::Object(mut top) = value else {
        return None;
    };
    let mut data = top.remove("data").unwrap_or(Value::Null);
    let own_next = take_array(&mut top, "next");
    let mut own_warnings = take_array(&mut top, "warnings");
    top.insert("truncated".to_string(), Value::Bool(true));
    // Under a ceiling the whole answer may be out of reach: ask for the
    // ceiling once, and at the ceiling ask for nothing more, only for a
    // narrower question.
    let overflow = Overflow { whole, max_bytes };
    let hint = match overflow.rerun_budget() {
        Some(budget) => Some(json!({
            "run": rerun_with(rerun, budget),
            "why": overflow.out_of_reach().unwrap_or_else(|| {
                format!("the whole answer is {whole} bytes and this budget was {max_bytes}")
            }),
            "read_only": true,
        })),
        None => {
            if let Some(why) = overflow.out_of_reach() {
                own_warnings.insert(0, Value::String(why));
            }
            None
        }
    };

    let mut cuts: Vec<Cut> = Vec::new();
    let mut keep_next = own_next.len();
    let mut keep_warnings = own_warnings.len();
    loop {
        // Measure the envelope with `data` as `null`, then swap in the size
        // of the real `data`, so the loop never copies a big answer.
        let mut shell = top.clone();
        let mut next: Vec<Value> = hint.iter().cloned().collect();
        next.extend_from_slice(&own_next[..keep_next]);
        let mut warnings = own_warnings[..keep_warnings].to_vec();
        warnings.extend(cut_notes(&cuts, &data).into_iter().map(Value::String));
        shell.insert("next".to_string(), Value::Array(next));
        shell.insert("warnings".to_string(), Value::Array(warnings));
        shell.insert("data".to_string(), Value::Null);
        let mut shell = Value::Object(shell);
        let size = line_length(&shell) - "null".len() + json_length(&data);
        if size <= max_bytes {
            shell["data"] = data;
            return Some(shell);
        }
        let excess = size - max_bytes;
        if cut_largest_array(&mut data, excess, &mut cuts)
            || shorten_longest_string(&mut data, excess, Some(&mut cuts))
        {
            continue;
        }
        if keep_next > 0 {
            keep_next -= 1;
        } else if keep_warnings > 0 {
            keep_warnings -= 1;
        } else {
            return None;
        }
    }
}

/// Shorten the strings in a failure's `error` until the envelope fits.
///
/// A failure has no `data` and no `truncated` flag; the only thing that can
/// be long is a message quoting what the API said.
fn trim_failure(mut value: Value, max_bytes: usize) -> Option<Value> {
    loop {
        let size = line_length(&value);
        if size <= max_bytes {
            return Some(value);
        }
        let error = value.get_mut("error")?;
        if !shorten_longest_string(error, size - max_bytes, None) {
            return None;
        }
    }
}

/// A failure envelope, trimmed to fit.
fn failure_that_fits(
    command: CommandName,
    relish: &BuildInfo,
    error: AgentError,
    max_bytes: usize,
) -> Rendered {
    let exit = error.code.exit_status();
    let envelope: Envelope<()> = Envelope::failed(command, relish.clone(), error);
    // Serialising these plain structs can't fail: every map key is a string.
    let value = serde_json::to_value(&envelope).unwrap_or_default();
    // Every string in `error` shortens to a few dozen bytes, so with at
    // least MIN_MAX_BYTES this always fits; the property test holds us to it.
    let value = trim_failure(value.clone(), max_bytes).unwrap_or(value);
    Rendered {
        json: format!("{value}\n"),
        exit,
    }
}

/// Remove `key` from `object` and return its items, if it was an array.
fn take_array(object: &mut serde_json::Map<String, Value>, key: &str) -> Vec<Value> {
    match object.remove(key) {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// One warning per cut that still points at something in `data`.
fn cut_notes(cuts: &[Cut], data: &Value) -> Vec<String> {
    cuts.iter()
        .filter_map(|cut| {
            let kept = match (cut.kind, data.pointer(&cut.pointer)?) {
                (CutKind::Items, Value::Array(items)) => items.len(),
                (CutKind::Bytes, Value::String(text)) => {
                    text.len().saturating_sub(STRING_CUT_MARKER.len())
                }
                _ => return None,
            };
            let unit = match cut.kind {
                CutKind::Items => format!("kept {kept} of {} items", cut.original),
                CutKind::Bytes => format!("kept the first {kept} of {} bytes", cut.original),
            };
            Some(format!("/data{}: {unit}", cut.pointer))
        })
        .collect()
}

/// Note a cut at `pointer`, keeping the original size from an earlier cut
/// in the same place.
fn record_cut(cuts: &mut Vec<Cut>, pointer: String, original: usize, kind: CutKind) {
    if !cuts
        .iter()
        .any(|cut| cut.pointer == pointer && cut.kind == kind)
    {
        cuts.push(Cut {
            pointer,
            original,
            kind,
        });
    }
}

/// Drop items from the end of the biggest non-empty array in `data`, and
/// say whether there was one.
///
/// It cuts until `excess` bytes are gone, or the array is no bigger than the
/// next biggest one outside it, whichever comes first, but always at least
/// half of `excess`. So one huge list doesn't lose everything while a
/// smaller one beside it stays whole: two big lists end up cut to about the
/// same size, in a handful of passes.
fn cut_largest_array(data: &mut Value, excess: usize, cuts: &mut Vec<Cut>) -> bool {
    let mut arrays = Vec::new();
    find_arrays(data, &mut String::new(), &mut arrays);
    let Some((size, pointer)) = arrays.iter().rev().max_by_key(|(size, _)| *size).cloned() else {
        return false;
    };
    let inside = format!("{pointer}/");
    let next = arrays
        .iter()
        .filter(|(_, other)| *other != pointer && !other.starts_with(&inside))
        .map(|(size, _)| *size)
        .max();
    let target = match next {
        Some(next) => excess.min(size - next).max(excess.div_ceil(2)),
        None => excess,
    };
    let Some(Value::Array(items)) = data.pointer_mut(&pointer) else {
        return false;
    };
    let original = items.len();
    let mut removed = 0;
    while removed < target {
        let Some(last) = items.pop() else {
            break;
        };
        // The item and the comma before it.
        removed += json_length(&last) + 1;
    }
    record_cut(cuts, pointer, original, CutKind::Items);
    true
}

/// Every non-empty array in `value`, as (size in bytes, JSON Pointer).
fn find_arrays(value: &Value, pointer: &mut String, arrays: &mut Vec<(usize, String)>) {
    match value {
        Value::Array(items) => {
            if !items.is_empty() {
                arrays.push((json_length(value), pointer.clone()));
            }
            for (index, item) in items.iter().enumerate() {
                let length = pointer.len();
                pointer.push_str(&format!("/{index}"));
                find_arrays(item, pointer, arrays);
                pointer.truncate(length);
            }
        }
        Value::Object(fields) => {
            for (key, field) in fields {
                let length = pointer.len();
                pointer.push('/');
                pointer.push_str(&escape_pointer_token(key));
                find_arrays(field, pointer, arrays);
                pointer.truncate(length);
            }
        }
        _ => {}
    }
}

/// Shorten the longest string in `value` (over [`SHORTEST_CUT_STRING`]
/// bytes) by about `excess` bytes, at a character boundary, and mark the
/// cut. `false` when no string is long enough to shorten.
fn shorten_longest_string(value: &mut Value, excess: usize, cuts: Option<&mut Vec<Cut>>) -> bool {
    let mut longest = None;
    find_longest_string(value, &mut String::new(), &mut longest);
    let Some((original, pointer)) = longest else {
        return false;
    };
    let Some(Value::String(text)) = value.pointer_mut(&pointer) else {
        return false;
    };
    let target = original
        .saturating_sub(excess + STRING_CUT_MARKER.len())
        .max(SHORTEST_KEPT_PREFIX);
    // `floor_char_boundary` steps back to the start of a UTF-8 character, so
    // the cut never splits one (which `truncate` would panic on).
    let keep = text.floor_char_boundary(target);
    text.truncate(keep);
    text.push_str(STRING_CUT_MARKER);
    if let Some(cuts) = cuts {
        record_cut(cuts, pointer, original, CutKind::Bytes);
    }
    true
}

fn find_longest_string(value: &Value, pointer: &mut String, longest: &mut Option<(usize, String)>) {
    match value {
        Value::String(text) => {
            if text.len() > SHORTEST_CUT_STRING
                && longest.as_ref().is_none_or(|(best, _)| text.len() > *best)
            {
                *longest = Some((text.len(), pointer.clone()));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let length = pointer.len();
                pointer.push_str(&format!("/{index}"));
                find_longest_string(item, pointer, longest);
                pointer.truncate(length);
            }
        }
        Value::Object(fields) => {
            for (key, field) in fields {
                let length = pointer.len();
                pointer.push('/');
                pointer.push_str(&escape_pointer_token(key));
                find_longest_string(field, pointer, longest);
                pointer.truncate(length);
            }
        }
        _ => {}
    }
}

/// A key as one JSON Pointer token: `~` and `/` are escaped (RFC 6901).
fn escape_pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// `bytes` rounded up to a whole KiB, a friendlier number for a hint.
fn round_up_to_kib(bytes: usize) -> usize {
    bytes.div_ceil(1024) * 1024
}

/// Counts bytes instead of storing them, to measure JSON without building it.
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The length of `value` as compact JSON.
fn json_length(value: &Value) -> usize {
    let mut counter = ByteCounter(0);
    // Writing a `Value` can't fail, and the counter never does.
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

/// The length of `value` as one printed line: compact JSON and a newline.
fn line_length(value: &Value) -> usize {
    json_length(value) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::agent::envelope::NextStep;
    use proptest::prelude::*;

    fn fixture_build() -> BuildInfo {
        BuildInfo {
            version: "0.2.7".to_string(),
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
            dirty: None,
            source_digest: None,
        }
    }

    fn render_json<T: Serialize>(envelope: &Envelope<T>, max_bytes: usize, rerun: &str) -> String {
        render(envelope, max_bytes, rerun).json
    }

    fn answered(data: Value) -> Envelope<Value> {
        Envelope::answered(CommandName::Version, fixture_build(), data)
    }

    fn parse(text: &str) -> Value {
        assert!(text.ends_with('\n'), "output ends in a newline");
        assert_eq!(text.matches('\n').count(), 1, "output is one line");
        serde_json::from_str(text).expect("output is valid JSON")
    }

    #[test]
    fn an_answer_within_budget_is_printed_whole() {
        let text = render_json(
            &answered(json!({"apps": [1, 2, 3]})),
            4096,
            "relish agent version",
        );
        let value = parse(&text);
        assert_eq!(value["truncated"], json!(false));
        assert_eq!(value["data"]["apps"], json!([1, 2, 3]));
        assert_eq!(value["next"], json!([]));
    }

    #[test]
    fn a_long_array_is_cut_from_the_end_and_says_so() {
        let apps: Vec<Value> = (0..500)
            .map(|index| json!({"app": format!("app-{index}"), "running": index}))
            .collect();
        let text = render_json(
            &answered(json!({"apps": apps})),
            2048,
            "relish agent version",
        );
        assert!(text.len() <= 2048, "{} bytes", text.len());
        let value = parse(&text);
        assert_eq!(value["truncated"], json!(true));
        let kept = value["data"]["apps"].as_array().unwrap();
        assert!(!kept.is_empty(), "keeps what fits");
        assert_eq!(kept[0]["app"], json!("app-0"), "keeps the front");
        let warnings = value["warnings"].as_array().unwrap();
        let note = format!("/data/apps: kept {} of 500 items", kept.len());
        assert!(warnings.contains(&json!(note)), "{warnings:?}");
        let hint = &value["next"][0];
        assert!(
            hint["run"]
                .as_str()
                .unwrap()
                .starts_with("relish agent version --max-bytes "),
            "{hint}"
        );
        assert_eq!(hint["read_only"], json!(true));
    }

    #[test]
    fn a_huge_list_beside_a_big_one_doesnt_lose_everything() {
        let apps: Vec<Value> = (0..2000)
            .map(|index| json!({"app": format!("app-{index:04}")}))
            .collect();
        let events: Vec<Value> = (0..200)
            .map(|index| json!({"event": format!("event-{index:03}")}))
            .collect();
        let text = render_json(
            &answered(json!({"apps": apps, "events": events})),
            4096,
            "relish agent version",
        );
        assert!(text.len() <= 4096, "{} bytes", text.len());
        let value = parse(&text);
        let kept_apps = value["data"]["apps"].as_array().unwrap().len();
        let kept_events = value["data"]["events"].as_array().unwrap().len();
        assert!(
            kept_apps > 0 && kept_events > 0,
            "{kept_apps} apps, {kept_events} events"
        );
        assert_eq!(value["data"]["apps"][0]["app"], json!("app-0000"));
    }

    #[test]
    fn the_rerun_hint_asks_for_enough_to_get_everything() {
        let apps: Vec<Value> = (0..500).map(|index| json!(index)).collect();
        let envelope = answered(json!({ "apps": apps }));
        let whole = render_json(&envelope, usize::MAX, "relish agent version").len();
        let text = render_json(&envelope, 1024, "relish agent version");
        let value = parse(&text);
        let run = value["next"][0]["run"].as_str().unwrap();
        let asked: usize = run.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(asked >= whole, "asked {asked}, whole answer {whole}");
        let again = render_json(&envelope, asked, "relish agent version");
        assert_eq!(parse(&again)["truncated"], json!(false));
    }

    /// Through `relish mcp` no answer is bigger than its ceiling, so a hint
    /// asking for the whole answer sent a model round in circles: it asked,
    /// got the ceiling, and was told to ask again (#1687).
    #[tokio::test]
    async fn under_a_ceiling_the_hint_asks_for_the_ceiling_and_says_to_narrow() {
        let apps: Vec<Value> = (0..2000).map(|index| json!(index)).collect();
        let envelope = answered(json!({ "apps": apps }));
        let text = within_ceiling(4096, async {
            render_json(&envelope, 2048, "relish agent version")
        })
        .await;
        let value = parse(&text);
        assert!(text.len() <= 2048, "{} bytes", text.len());
        let hint = &value["next"][0];
        assert_eq!(hint["run"], json!("relish agent version --max-bytes 4096"));
        assert!(hint["why"].as_str().unwrap().contains("narrow"), "{hint}");
    }

    #[tokio::test]
    async fn at_the_ceiling_no_step_asks_for_more_and_a_warning_says_to_narrow() {
        let apps: Vec<Value> = (0..2000).map(|index| json!(index)).collect();
        let envelope = answered(json!({ "apps": apps }));
        let text = within_ceiling(4096, async {
            render_json(&envelope, 4096, "relish agent version")
        })
        .await;
        let value = parse(&text);
        assert_eq!(value["truncated"], json!(true));
        for step in value["next"].as_array().unwrap() {
            assert!(
                !step["run"].as_str().unwrap().contains("--max-bytes"),
                "{step}"
            );
        }
        let warnings = value["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("narrow")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn under_a_ceiling_an_answer_that_fits_it_keeps_the_usual_hint() {
        let apps: Vec<Value> = (0..500).map(|index| json!(index)).collect();
        let envelope = answered(json!({ "apps": apps }));
        let whole = render_json(&envelope, usize::MAX, "relish agent version").len();
        let text = within_ceiling(64 * 1024, async {
            render_json(&envelope, 1024, "relish agent version")
        })
        .await;
        let hint = &parse(&text)["next"][0];
        assert_eq!(
            hint["run"],
            json!(format!(
                "relish agent version --max-bytes {}",
                round_up_to_kib(whole)
            ))
        );
        assert!(!hint["why"].as_str().unwrap().contains("narrow"), "{hint}");
    }

    #[test]
    fn a_long_string_is_shortened_at_a_character_boundary() {
        let text_value = "é".repeat(5000);
        let text = render_json(
            &answered(json!({ "text": text_value })),
            2048,
            "relish agent version",
        );
        assert!(text.len() <= 2048, "{} bytes", text.len());
        let value = parse(&text);
        assert_eq!(value["truncated"], json!(true));
        let kept = value["data"]["text"].as_str().unwrap();
        assert!(kept.ends_with(STRING_CUT_MARKER), "{kept}");
        let warnings = value["warnings"].as_array().unwrap();
        assert!(
            warnings.iter().any(|note| note
                .as_str()
                .unwrap()
                .starts_with("/data/text: kept the first ")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_commands_own_next_steps_go_before_its_warnings_when_cutting() {
        let mut envelope = answered(json!({}));
        for index in 0..100 {
            envelope = envelope
                .with_next(NextStep {
                    run: format!("relish agent version --step {index}"),
                    why: "x".repeat(40),
                    read_only: true,
                })
                .with_warning(format!("warning {index}"));
        }
        let text = render_json(&envelope, 2048, "relish agent version");
        assert!(text.len() <= 2048);
        let value = parse(&text);
        assert_eq!(value["truncated"], json!(true));
        assert!(
            value["next"][0]["run"]
                .as_str()
                .unwrap()
                .contains("--max-bytes")
        );
    }

    #[test]
    fn a_failure_with_a_huge_message_still_fits() {
        let error = AgentError::new(ErrorCode::Api, "x".repeat(100_000));
        let envelope: Envelope<Value> =
            Envelope::failed(CommandName::Version, fixture_build(), error);
        let text = render_json(&envelope, 1024, "relish agent version");
        assert!(text.len() <= 1024, "{} bytes", text.len());
        let value = parse(&text);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error"]["code"], json!("api"));
        assert!(
            value.get("truncated").is_none(),
            "a failure has no truncated"
        );
    }

    #[test]
    fn an_answer_that_cannot_be_trimmed_to_fit_becomes_a_usage_failure() {
        // Objects aren't trimmed (their keys are the schema), so a thousand
        // short fields can't shrink.
        let mut fields = serde_json::Map::new();
        for index in 0..1000 {
            fields.insert(format!("field_{index:04}"), json!(index));
        }
        let text = render_json(
            &answered(Value::Object(fields)),
            2048,
            "relish agent version",
        );
        assert!(text.len() <= 2048);
        let value = parse(&text);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error"]["code"], json!("usage"));
        assert!(
            value["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("--max-bytes"),
            "{value}"
        );
    }

    #[test]
    fn a_budget_below_the_minimum_counts_as_the_minimum() {
        let apps: Vec<Value> = (0..500).map(|index| json!(index)).collect();
        let text = render_json(
            &answered(json!({ "apps": apps })),
            10,
            "relish agent version",
        );
        assert!(text.len() <= MIN_MAX_BYTES);
        assert_eq!(parse(&text)["truncated"], json!(true));
    }

    /// Any JSON value: nested arrays and objects of strings (including
    /// multi-byte and control characters), numbers, booleans and nulls.
    fn arbitrary_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|number| json!(number)),
            ".{0,300}".prop_map(Value::String),
            "[\\x00-\\x1f\"\\\\é€😀]{0,200}".prop_map(Value::String),
        ];
        leaf.prop_recursive(5, 2000, 60, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..60).prop_map(Value::Array),
                prop::collection::btree_map("[a-z_]{1,12}", inner, 0..12)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn no_output_exceeds_its_budget(
            data in arbitrary_json(),
            next in prop::collection::vec((".{0,120}", ".{0,120}", any::<bool>()), 0..20),
            warnings in prop::collection::vec(".{0,200}", 0..20),
            max_bytes in MIN_MAX_BYTES..(64 * 1024),
            message in prop::option::of(".{0,20000}"),
        ) {
            let envelope = match message {
                None => {
                    let mut envelope = answered(data);
                    for (run, why, read_only) in next {
                        envelope = envelope.with_next(NextStep { run, why, read_only });
                    }
                    for warning in warnings {
                        envelope = envelope.with_warning(warning);
                    }
                    envelope
                }
                Some(message) => Envelope::failed(
                    CommandName::Version,
                    fixture_build(),
                    AgentError::new(ErrorCode::Api, message).with_hint("h".repeat(3000)),
                ),
            };
            let text = render_json(&envelope, max_bytes, "relish agent version");
            prop_assert!(text.len() <= max_bytes, "{} > {}", text.len(), max_bytes);
            let value: Value = serde_json::from_str(&text).expect("valid JSON");
            prop_assert_eq!(&value["schema"], &json!("relish.agent/v1"));
        }
    }

    /// Every "ask for more" step goes through [`Overflow::rerun_budget`], so
    /// no command can name a budget above the ceiling `relish mcp` holds
    /// answers to. `logs` and `manual show` each built their own and
    /// bypassed it (#1818's review); this catches the next one.
    #[test]
    fn no_command_writes_its_own_max_bytes_step() {
        fn sources(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/relish");
        let mut found = Vec::new();
        sources(&root, &mut found);
        let offenders: Vec<_> = found
            .iter()
            .filter(|path| !path.ends_with("agent/budget.rs"))
            .filter(|path| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .contains("--max-bytes {")
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "build the step with budget::Overflow instead: {offenders:?}"
        );
    }
}
