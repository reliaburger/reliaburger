//! Reading what cargo says: its JSON messages (`--message-format=json`)
//! for compiler errors and built executables, and the test harness's text
//! for results.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How many compiler errors an answer lists; `total_errors` counts them all.
pub const SHOWN_ERRORS: usize = 20;

/// The longest `rendered` diagnostic an answer carries, in bytes.
const RENDERED_LIMIT: usize = 1500;

/// The longest captured output a failed test carries, in bytes.
const FAILURE_OUTPUT_LIMIT: usize = 2000;

/// One error the compiler reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompilerError {
    /// The file, relative to the workspace, when the error points at one.
    pub file: Option<String>,
    /// The line in `file`.
    pub line: Option<u64>,
    /// The column in `file`.
    pub column: Option<u64>,
    /// The error code, such as `E0308`, when it has one.
    pub code: Option<String>,
    /// The one-line message.
    pub message: String,
    /// The error as the compiler prints it, with the source lines it points
    /// at, cut at 1500 bytes.
    pub rendered: Option<String>,
}

/// What cargo's JSON messages said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Messages {
    /// Every distinct compiler error, in the order cargo reported them.
    pub errors: Vec<CompilerError>,
    /// Each executable built: `(target name, path)`.
    pub executables: Vec<(String, String)>,
}

/// The wire shapes of the messages we read. Anything else in a message is
/// ignored, and so is a message that doesn't parse.
#[derive(Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum CargoLine {
    CompilerMessage {
        message: Diagnostic,
    },
    CompilerArtifact {
        target: Target,
        executable: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Diagnostic {
    message: String,
    level: String,
    code: Option<DiagnosticCode>,
    spans: Vec<Span>,
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct DiagnosticCode {
    code: String,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    line_start: u64,
    column_start: u64,
    is_primary: bool,
}

#[derive(Deserialize)]
struct Target {
    name: String,
}

/// Read cargo's stdout. Lines that aren't JSON (a test harness's output,
/// when `cargo test` runs) are skipped.
pub fn parse_messages(stdout: &str) -> Messages {
    let mut messages = Messages::default();
    let mut seen = HashSet::new();
    for line in stdout.lines().filter(|line| line.starts_with('{')) {
        match serde_json::from_str::<CargoLine>(line) {
            Ok(CargoLine::CompilerMessage { message }) => {
                let Some(error) = compiler_error(message) else {
                    continue;
                };
                // The same error comes once per target that compiles the
                // file: the library, then its tests.
                let key = (
                    error.file.clone(),
                    error.line,
                    error.column,
                    error.message.clone(),
                );
                if seen.insert(key) {
                    messages.errors.push(error);
                }
            }
            Ok(CargoLine::CompilerArtifact {
                target,
                executable: Some(path),
            }) => messages.executables.push((target.name, path)),
            _ => {}
        }
    }
    messages
}

/// A diagnostic as a [`CompilerError`], or `None` when it isn't an error or
/// is only the summary ("aborting due to 2 previous errors").
fn compiler_error(diagnostic: Diagnostic) -> Option<CompilerError> {
    if !diagnostic.level.starts_with("error") || diagnostic.message.starts_with("aborting due to") {
        return None;
    }
    let primary = diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or(diagnostic.spans.first());
    Some(CompilerError {
        file: primary.map(|span| span.file_name.clone()),
        line: primary.map(|span| span.line_start),
        column: primary.map(|span| span.column_start),
        code: diagnostic.code.map(|code| code.code),
        message: diagnostic.message,
        rendered: diagnostic
            .rendered
            .map(|text| cut(text.trim_end(), RENDERED_LIMIT)),
    })
}

/// `text` cut at a character boundary at most `limit` bytes in, marked when
/// it was cut.
pub fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[cut]", &text[..end])
}

/// The last `count` lines of `text`, for an error with nothing better to
/// show, such as cargo refusing to start.
pub fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// Cargo's own failure, when it stopped before the compiler said anything:
/// an unresolvable dependency, a lock file out of date, a manifest it
/// couldn't find. The message is its first `error:` line, so an answer with
/// `ok: false` always has an error to read (#1882); `rendered` is the end of
/// what it printed.
pub fn cargo_error(stderr: &str) -> CompilerError {
    let message = stderr
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("error: "))
        .or_else(|| {
            stderr
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .unwrap_or("cargo failed and printed nothing");
    let rendered = last_lines(stderr.trim_end(), 20);
    CompilerError {
        file: None,
        line: None,
        column: None,
        code: None,
        message: message.to_string(),
        rendered: (!rendered.is_empty()).then_some(rendered),
    }
}

/// What the test harness printed, added up over every test binary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestResults {
    /// Tests that passed.
    pub passed: u64,
    /// Tests that failed.
    pub failed: u64,
    /// Tests marked `#[ignore]`.
    pub ignored: u64,
    /// Tests the filter left out.
    pub filtered_out: u64,
    /// How many test binaries reported a result.
    pub binaries: u64,
    /// Each failed test with what it printed, in the order they failed.
    pub failures: Vec<(String, String)>,
}

/// Read the libtest output of `cargo test`.
pub fn parse_test_output(stdout: &str) -> TestResults {
    let mut results = TestResults::default();
    let mut current: Option<(String, Vec<&str>)> = None;
    let mut outputs: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            if let Some((name, lines)) = current.take() {
                outputs.push((name, lines.join("\n")));
            }
            current = Some((name.to_string(), Vec::new()));
            continue;
        }
        if (line == "failures:" || line.starts_with("test result: "))
            && let Some((name, lines)) = current.take()
        {
            outputs.push((name, lines.join("\n")));
        }
        if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
            continue;
        }
        if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            results.failures.push((name.to_string(), String::new()));
        }
        if let Some(summary) = line.strip_prefix("test result: ") {
            add_summary(&mut results, summary);
        }
    }
    for (name, output) in &mut results.failures {
        if let Some((_, text)) = outputs.iter().find(|(failed, _)| failed == name) {
            *output = cut(text.trim(), FAILURE_OUTPUT_LIMIT);
        }
    }
    results
}

/// Add one `ok. 3 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out;
/// finished in 0.01s` to the totals.
fn add_summary(results: &mut TestResults, summary: &str) {
    results.binaries += 1;
    for part in summary.split(';') {
        let mut words = part.split_whitespace().rev();
        let Some(label) = words.next() else {
            continue;
        };
        let (label, count) = match label {
            "out" => ("filtered out", words.nth(1)),
            other => (other, words.next()),
        };
        let Some(count) = count.and_then(|count| count.parse::<u64>().ok()) else {
            continue;
        };
        match label {
            "passed" => results.passed += count,
            "failed" => results.failed += count,
            "ignored" => results.ignored += count,
            "filtered out" => results.filtered_out += count,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real messages from cargo 1.9x, shortened.
    const TYPE_ERROR: &str = r#"{"reason":"compiler-message","package_id":"path+file:///w#reliaburger@0.2.7","manifest_path":"/w/Cargo.toml","target":{"kind":["lib"],"name":"reliaburger"},"message":{"rendered":"error[E0308]: mismatched types\n --> src/relish/client.rs:12:5\n   |\n12 |     \"no\"\n   |     ^^^^ expected `u32`, found `&str`\n\n","$message_type":"diagnostic","children":[],"code":{"code":"E0308","explanation":"Expected type did not match the received type.\n"},"level":"error","message":"mismatched types","spans":[{"byte_end":120,"byte_start":116,"column_end":9,"column_start":5,"expansion":null,"file_name":"src/relish/client.rs","is_primary":true,"label":"expected `u32`, found `&str`","line_end":12,"line_start":12,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;
    const WARNING: &str = r#"{"reason":"compiler-message","target":{"name":"reliaburger"},"message":{"rendered":"warning: unused variable","code":null,"level":"warning","message":"unused variable: `x`","spans":[{"file_name":"src/lib.rs","line_start":3,"column_start":9,"is_primary":true}]}}"#;
    const ABORTING: &str = r#"{"reason":"compiler-message","target":{"name":"reliaburger"},"message":{"rendered":"error: aborting due to 1 previous error\n","code":null,"level":"error","message":"aborting due to 1 previous error","spans":[]}}"#;
    const ARTIFACT: &str = r#"{"reason":"compiler-artifact","package_id":"x","target":{"kind":["bin"],"name":"relish"},"profile":{},"features":[],"filenames":["/w/target/debug/relish"],"executable":"/w/target/debug/relish","fresh":false}"#;
    const LIBRARY: &str = r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"reliaburger"},"filenames":["/w/target/debug/libreliaburger.rlib"],"executable":null,"fresh":true}"#;
    const FINISHED: &str = r#"{"reason":"build-finished","success":false}"#;

    #[test]
    fn compiler_errors_come_with_their_file_line_and_code() {
        let stdout = [TYPE_ERROR, WARNING, ABORTING, FINISHED].join("\n");
        let messages = parse_messages(&stdout);
        assert_eq!(messages.errors.len(), 1, "{messages:?}");
        let error = &messages.errors[0];
        assert_eq!(error.file.as_deref(), Some("src/relish/client.rs"));
        assert_eq!((error.line, error.column), (Some(12), Some(5)));
        assert_eq!(error.code.as_deref(), Some("E0308"));
        assert_eq!(error.message, "mismatched types");
        assert!(
            error
                .rendered
                .as_deref()
                .unwrap()
                .contains("expected `u32`")
        );
    }

    #[test]
    fn the_same_error_from_two_targets_is_listed_once() {
        let test_target = TYPE_ERROR.replace(r#""kind":["lib"]"#, r#""kind":["test"]"#);
        let messages = parse_messages(&[TYPE_ERROR, &test_target].join("\n"));
        assert_eq!(messages.errors.len(), 1);
    }

    #[test]
    fn executables_come_from_artifacts_and_libraries_are_skipped() {
        let stdout = ["   Compiling reliaburger v0.2.7", LIBRARY, ARTIFACT].join("\n");
        let messages = parse_messages(&stdout);
        assert_eq!(
            messages.executables,
            [("relish".to_string(), "/w/target/debug/relish".to_string())]
        );
        assert!(messages.errors.is_empty());
    }

    #[test]
    fn a_long_rendered_error_is_cut_on_a_character_boundary() {
        let long = "é".repeat(2000);
        let cut_text = cut(&long, 1501);
        assert!(cut_text.ends_with("…[cut]"));
        assert!(cut_text.len() <= 1501 + "…[cut]".len());
        assert_eq!(cut("short", 10), "short");
    }

    const TEST_OUTPUT: &str = "\
running 3 tests
test relish::client::tests::a_timeout_is_a_timeout ... ok
test relish::client::tests::a_refusal_is_unreachable ... FAILED
test relish::client::tests::slow ... ignored, run with make test-slow

failures:

---- relish::client::tests::a_refusal_is_unreachable stdout ----

thread 'relish::client::tests::a_refusal_is_unreachable' panicked at src/relish/client.rs:40:9:
assertion `left == right` failed
  left: Timeout
 right: Unreachable


failures:
    relish::client::tests::a_refusal_is_unreachable

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 12 filtered out; finished in 0.02s

running 2 tests
test suite::cli::works ... ok
test suite::cli::also_works ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
";

    #[test]
    fn test_results_add_up_over_every_binary() {
        let results = parse_test_output(TEST_OUTPUT);
        assert_eq!(results.passed, 3);
        assert_eq!(results.failed, 1);
        assert_eq!(results.ignored, 1);
        assert_eq!(results.filtered_out, 12);
        assert_eq!(results.binaries, 2);
    }

    #[test]
    fn a_failed_test_carries_what_it_printed() {
        let results = parse_test_output(TEST_OUTPUT);
        assert_eq!(results.failures.len(), 1);
        let (name, output) = &results.failures[0];
        assert_eq!(name, "relish::client::tests::a_refusal_is_unreachable");
        assert!(output.starts_with("thread '"), "{output}");
        assert!(output.contains("right: Unreachable"), "{output}");
        assert!(!output.contains("failures:"), "{output}");
    }

    #[test]
    fn no_harness_output_is_no_results() {
        assert_eq!(parse_test_output(""), TestResults::default());
    }

    #[test]
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(last_lines("a", 5), "a");
    }

    #[test]
    fn cargos_own_error_is_its_first_error_line() {
        let stderr = "    Updating crates.io index\n\
                      error: failed to select a version for `tokio`\n\
                      \x20   ... required by package `reliaburger`\n";
        let error = cargo_error(stderr);
        assert_eq!(error.message, "failed to select a version for `tokio`");
        assert_eq!(error.rendered.as_deref(), Some(stderr.trim_end()));
        assert_eq!((error.file, error.line, error.code), (None, None, None));
    }

    #[test]
    fn cargo_failing_with_no_error_line_still_says_so() {
        assert_eq!(cargo_error("").message, "cargo failed and printed nothing");
        assert_eq!(cargo_error("Killed\n").message, "Killed");
    }

    /// A test that runs its own binary again with stdout inherited puts a
    /// second `test result:` line in cargo's output, and `patch test` adds
    /// it to the totals: four such children once made a whole-suite run
    /// report `passed` 4 too high and `filtered_out` at 31,587 (#1882).
    /// Every re-run in the portable suite's sources takes its child's
    /// stdout: with `.stdout(...)`, or `.output()`, which pipes it.
    #[test]
    fn no_portable_test_lets_a_rerun_of_its_binary_print_into_the_totals() {
        fn sources(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src", "tests/suite", "tests/support"] {
            sources(&root.join(dir), &mut files);
        }
        let mut leaking = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                // Split, so this line doesn't find itself.
                if !line.contains(concat!("Command::new(std::env::", "current_exe()")) {
                    continue;
                }
                let chain: Vec<&str> = lines[index..]
                    .iter()
                    .take(40)
                    .copied()
                    .take_while(|line| !line.contains(".spawn()") && !line.contains(".status()"))
                    .collect();
                let rest = &lines[index + chain.len()..];
                let starts = rest
                    .first()
                    .is_some_and(|line| line.contains(".spawn()") || line.contains(".status()"));
                let takes_stdout = chain
                    .iter()
                    .any(|line| line.contains(".stdout(") || line.contains(".output()"));
                if starts && !takes_stdout {
                    leaking.push(format!(
                        "{}:{}",
                        file.strip_prefix(root).unwrap().display(),
                        index + 1
                    ));
                }
            }
        }
        assert!(leaking.is_empty(), "{leaking:?}");
    }
}
