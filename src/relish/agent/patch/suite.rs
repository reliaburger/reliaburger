//! `relish agent patch test`: the portable suite in the workspace, and an
//! answer that says it was only the portable suite.

use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;

use super::builder::{check_build_changes, check_disk, run_cargo};
use super::cargo::{CompilerError, SHOWN_ERRORS, last_lines, parse_messages, parse_test_output};
use super::git::snapshot;
use super::workspace::TestRun;
use super::{Tools, answer_of, open, patch_command, shown, step};
use crate::relish::agent::envelope::{AgentError, ErrorCode};
use crate::relish::agent::source::Answer;

/// How long a test run may take, compiling included.
const TEST_DEADLINE: Duration = Duration::from_secs(90 * 60);

/// How many failed tests an answer lists.
const SHOWN_FAILURES: usize = 10;

/// What didn't run, in every answer, so nobody reads "passed" as more than
/// it is.
pub const NOT_RUN: &str = "only the portable suite ran: unit tests, tests/suite, every other \
     test that isn't #[ignore], and the doctests, as make test-cargo runs them. The gated \
     suites (make test-cluster, test-linux, test-slow, test-upgrade and the rest) are \
     marked ignore, so they didn't run. Nor did the repository checks in tests/suite \
     (agent_docs, doc_snippets, documented_manifests, issue_forms, toolchain_pins, website, \
     website_markdown and one cloud_init test): they check the website, the workflows and \
     the issue forms, which the source bundle doesn't carry";

/// The tests `patch test` skips, as libtest `--skip` patterns: the
/// repository checks in `tests/suite`. They read the website, the GitHub
/// workflows and issue forms, the READMEs and the devcontainer, none of which
/// the source bundle carries, so in a workspace they could only fail
/// (#1876). [`NOT_RUN`] names them.
pub const REPOSITORY_CHECKS: &[&str] = &[
    "agent_docs::",
    "cloud_init::the_install_line_uses_a_flag_the_installer_has",
    "doc_snippets::",
    "documented_manifests::",
    "issue_forms::",
    "toolchain_pins::",
    "website::",
    "website_markdown::",
];

/// Which suite ran. One variant, so the schema says exactly which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    /// `cargo test`: everything not marked `#[ignore]`.
    Portable,
}

/// One test that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct TestFailure {
    /// Its full name, e.g. `relish::client::tests::a_timeout_is_a_timeout`.
    pub name: String,
    /// What it printed, panic message included, cut at 2000 bytes.
    pub output: String,
}

/// `data` for `relish agent patch test`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TestData {
    /// The workspace.
    pub dir: String,
    /// Always `portable`.
    pub suite: Suite,
    /// What this run didn't cover.
    pub not_run: String,
    /// The filter, or `null` for the whole suite.
    pub filter: Option<String>,
    /// Whether it compiled and every test that ran passed: the verdict, since
    /// the exit code is 0 either way. `false` is still an answer: `failures`
    /// or `errors` says why.
    pub ok: bool,
    /// Tests that passed.
    pub passed: u64,
    /// Tests that failed.
    pub failed: u64,
    /// Tests marked `#[ignore]`: the gated suites.
    pub ignored: u64,
    /// Tests the filter left out.
    pub filtered_out: u64,
    /// The first ten failed tests, with what each printed.
    pub failures: Vec<TestFailure>,
    /// The first 20 compiler errors, when the tests didn't compile.
    pub errors: Vec<CompilerError>,
    /// How many distinct compiler errors there were.
    pub total_errors: usize,
    /// The end of cargo's own output, when it failed with nothing above to
    /// show for it.
    pub cargo_output: Option<String>,
    /// The snapshot of the source that was tested (a git tree id).
    pub tree: String,
    /// How long it took, compiling included.
    pub seconds: f64,
}

/// `relish agent patch test`.
pub(super) async fn test(
    tools: &Tools,
    dir: &Path,
    filter: Option<&str>,
    allow_build_changes: bool,
) -> Result<Answer<TestData>, AgentError> {
    if filter.is_some_and(|filter| filter.starts_with('-')) {
        return Err(AgentError::new(
            ErrorCode::Usage,
            "the filter is a test name, not an option",
        ));
    }
    let mut state = open(dir)?;
    let tree = snapshot(dir)?;
    check_build_changes(dir, &state, &tree, allow_build_changes)?;
    check_disk(dir, tools.free_disk_needed)?;

    // `--no-fail-fast`: without it cargo stops at the first test binary
    // with a failure, and every binary after it goes uncounted (#1876).
    let mut args = vec![
        "test",
        "--locked",
        "--no-fail-fast",
        "--message-format=json",
    ];
    // After `--`, so cargo hands them to the test binaries: the filter, then
    // the repository checks to skip.
    args.push("--");
    args.extend(filter);
    for check in REPOSITORY_CHECKS {
        args.extend(["--skip", check]);
    }
    let run = run_cargo(tools, dir, &args, TEST_DEADLINE).await?;
    let messages = parse_messages(&run.stdout);
    let results = parse_test_output(&run.stdout);
    let ok = run.success && messages.errors.is_empty();
    let cargo_output = (!ok && messages.errors.is_empty() && results.failures.is_empty())
        .then(|| last_lines(&run.stderr, 20));

    state.record_test(TestRun {
        filter: filter.map(str::to_string),
        tree: tree.clone(),
        ok,
        passed: results.passed,
        failed: results.failed,
        ignored: results.ignored,
        seconds: run.seconds,
    });
    state.save(dir)?;

    let ran = results.passed + results.failed;
    let mut answer = answer_of(TestData {
        dir: shown(dir),
        suite: Suite::Portable,
        not_run: NOT_RUN.to_string(),
        filter: filter.map(str::to_string),
        ok,
        passed: results.passed,
        failed: results.failed,
        ignored: results.ignored,
        filtered_out: results.filtered_out,
        failures: results
            .failures
            .into_iter()
            .take(SHOWN_FAILURES)
            .map(|(name, output)| TestFailure { name, output })
            .collect(),
        total_errors: messages.errors.len(),
        errors: messages.errors.into_iter().take(SHOWN_ERRORS).collect(),
        cargo_output,
        tree,
        seconds: run.seconds,
    });
    if ok && ran == 0 {
        answer
            .warnings
            .push("no test ran: the filter matched none".to_string());
    }
    if ok {
        answer.next.push(step(
            patch_command("diff", dir),
            "the change and the tests that ran against it",
            true,
        ));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::*;
    use super::super::workspace::State;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use crate::relish::agent::envelope::CommandName;
    use crate::relish::agent::source::fixture as source_fixture;
    use crate::relish::agent::source::render_answer;

    /// A cargo whose suite has one failing test, and which records its
    /// arguments.
    const CARGO_ONE_FAILS: &str = r#"
echo "$@" > "$PWD/.reliaburger-patch/args"
cat <<'EOF'
{"reason":"compiler-artifact","target":{"name":"reliaburger"},"executable":null}
running 2 tests
test client::tests::a_timeout_is_a_timeout ... ok
test client::tests::a_refusal_is_unreachable ... FAILED

failures:

---- client::tests::a_refusal_is_unreachable stdout ----

thread 'client::tests::a_refusal_is_unreachable' panicked at src/client.rs:40:9:
assertion `left == right` failed
  left: Timeout
 right: Unreachable

failures:
    client::tests::a_refusal_is_unreachable

test result: FAILED. 1 passed; 1 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.02s
EOF
exit 101
"#;

    /// A cargo whose suite passes.
    const CARGO_PASSES: &str = r#"
echo "$@" > "$PWD/.reliaburger-patch/args"
echo 'running 1 test'
echo 'test client::tests::a_timeout_is_a_timeout ... ok'
echo 'test result: ok. 1 passed; 0 failed; 3 ignored; 0 measured; 9 filtered out; finished in 0.01s'
"#;

    fn rendered(result: Result<Answer<TestData>, AgentError>) -> crate::relish::agent::Rendered {
        render_answer(
            CommandName::PatchTest,
            source_fixture::bundle().build_info(),
            result,
            &[],
            DEFAULT_MAX_BYTES,
            "relish agent patch test",
        )
    }

    #[tokio::test]
    async fn snapshot_of_a_test_run_with_a_failure() {
        let fixture = Fixture::new(CARGO_ONE_FAILS);
        let result = test(&fixture.tools, &fixture.dir(), None, false).await;
        let rendered = rendered(result);
        assert_eq!(rendered.exit.code(), 0, "a failing test is still an answer");
        insta::assert_json_snapshot!(normalised(&rendered, fixture.root.path()));
    }

    #[tokio::test]
    async fn snapshot_of_a_filtered_test_run_that_passes() {
        let fixture = Fixture::new(CARGO_PASSES);
        let dir = fixture.dir();
        let result = test(&fixture.tools, &dir, Some("a_timeout"), false).await;
        let args = std::fs::read_to_string(dir.join(".reliaburger-patch/args")).unwrap();
        assert!(args.starts_with("test --manifest-path "), "{args}");
        assert!(
            args.contains(" --locked --no-fail-fast --message-format=json -- a_timeout --skip "),
            "{args}"
        );
        insta::assert_json_snapshot!(normalised(&rendered(result), fixture.root.path()));
    }

    #[tokio::test]
    async fn a_whole_suite_run_keeps_going_past_a_failure_and_skips_the_repository_checks() {
        let fixture = Fixture::new(CARGO_PASSES);
        let dir = fixture.dir();
        test(&fixture.tools, &dir, None, false).await.unwrap();
        let args = std::fs::read_to_string(dir.join(".reliaburger-patch/args")).unwrap();
        let skips: String = REPOSITORY_CHECKS
            .iter()
            .map(|check| format!(" --skip {check}"))
            .collect();
        assert!(
            args.ends_with(&format!(
                " --locked --no-fail-fast --message-format=json --{skips}\n"
            )),
            "{args}"
        );
    }

    /// A skip pattern for a module that was renamed would skip nothing, and
    /// `not_run` would name a check that ran.
    #[test]
    fn every_repository_check_is_a_suite_module_that_not_run_names() {
        let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/suite");
        let main = std::fs::read_to_string(suite.join("main.rs")).unwrap();
        for check in REPOSITORY_CHECKS {
            let (module, test) = check.split_once("::").unwrap();
            assert!(main.contains(&format!("\nmod {module};")), "{check}");
            assert!(NOT_RUN.contains(module), "not_run leaves out {module}");
            if !test.is_empty() {
                let code = std::fs::read_to_string(suite.join(format!("{module}.rs"))).unwrap();
                assert!(code.contains(&format!("fn {test}()")), "{check}");
            }
        }
    }

    #[tokio::test]
    async fn every_run_is_remembered_against_the_source_it_tested() {
        let fixture = Fixture::new(CARGO_PASSES);
        let dir = fixture.dir();
        test(&fixture.tools, &dir, None, false).await.unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub mod client;\n// fixed\n").unwrap();
        test(&fixture.tools, &dir, Some("client"), false)
            .await
            .unwrap();
        let state = State::load(&dir).unwrap();
        assert_eq!(state.tests.len(), 2);
        assert_ne!(state.tests[0].tree, state.tests[1].tree);
        assert_eq!(state.tests[1].filter.as_deref(), Some("client"));
        assert_eq!(state.tests[1].ignored, 3);
    }

    #[tokio::test]
    async fn a_filter_that_looks_like_an_option_is_a_usage_error() {
        let fixture = Fixture::new(CARGO_PASSES);
        let error = test(&fixture.tools, &fixture.dir(), Some("--release"), false)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
    }

    #[tokio::test]
    async fn a_filter_that_matches_nothing_warns() {
        let fixture = Fixture::new(
            "echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s'\n",
        );
        let answer = test(&fixture.tools, &fixture.dir(), Some("nope"), false)
            .await
            .unwrap();
        assert!(answer.data.ok);
        assert_eq!(answer.warnings, ["no test ran: the filter matched none"]);
    }

    #[tokio::test]
    async fn tests_refuse_a_change_to_the_build_too() {
        let fixture = Fixture::new(CARGO_PASSES);
        let dir = fixture.dir();
        std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
        let error = test(&fixture.tools, &dir, None, false).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Forbidden);
        assert!(test(&fixture.tools, &dir, None, true).await.is_ok());
    }
}
