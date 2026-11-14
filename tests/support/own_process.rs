//! Give a test a process of its own under plain `cargo test` (#1876).
//!
//! Some tests set or measure process-wide state: a `OnceLock` the first
//! caller wins, a static token, a counting global allocator. nextest runs
//! every test in its own process, so they pass under `make test`. Plain
//! `cargo test`, which `relish agent patch test` runs, puts every test of a
//! binary on a thread of one process, where they see each other's state.
//! [`ran_in_its_own_process`] runs such a test again, alone, in a child.

use std::time::Duration;

/// The variable a child finds the name of the test it should run in.
const CHILD_TEST: &str = "RELIABURGER_OWN_PROCESS_TEST";

/// How long the child may take.
const CHILD_DEADLINE: Duration = Duration::from_secs(300);

/// Whether this process is already the test's own: under nextest, or as the
/// child [`ran_in_its_own_process`] started for `test`.
pub fn has_its_own_process(test: &str) -> bool {
    std::env::var("NEXTEST_EXECUTION_MODE").as_deref() == Ok("process-per-test")
        || std::env::var(CHILD_TEST).as_deref() == Ok(test)
}

/// Run `test` (its full name in this test binary, as `--exact` takes it) in
/// a process of its own, unless it already has one.
///
/// `true` means a child ran it and it passed, so the caller returns at once;
/// `false` means this process is the test's own, so the caller runs its
/// body. A child that fails, or that passes anything but exactly one test,
/// fails the caller with the child's output.
pub async fn ran_in_its_own_process(test: &str) -> bool {
    if has_its_own_process(test) {
        return false;
    }
    let output = tokio::time::timeout(
        CHILD_DEADLINE,
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD_TEST, test)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("{test} ran past {CHILD_DEADLINE:?} in its own process"))
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `--exact` with a name that matches nothing runs no test and exits 0.
    assert!(
        output.status.success() && stdout.contains(" 1 passed;"),
        "{test} failed in its own process:\n{stdout}\n{stderr}"
    );
    true
}
