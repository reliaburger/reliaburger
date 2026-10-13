//! The catalogue of test cases, and how `--filter` selects from it.

use crate::bun::capabilities::Capability;

use super::context::TestContext;
use super::lease::LeaseScope;
use super::report::TestGroup;

/// A case's explicit failure or missing-evidence outcome.
/// Workload error strings always become `Failed`, regardless of their contents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaseError {
    /// An observed assertion or operation failed.
    #[error("{0}")]
    Failed(String),
    /// Available evidence cannot establish a verdict.
    #[error("{0}")]
    Unknown(String),
}

impl From<String> for CaseError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

/// A completed case, with typed failure and missing-evidence alternatives.
pub type CaseResult = Result<(), CaseError>;

/// Stop a case because it cannot establish pass or fail.
pub fn unknown(reason: impl std::fmt::Display) -> CaseResult {
    Err(CaseError::Unknown(reason.to_string()))
}

/// A test case body.
///
/// Rust has no `async fn` *pointers* — an `async fn` desugars to a function
/// returning an anonymous `impl Future`, and every one of those is a distinct
/// type, so they can't share a `fn` type the way ordinary functions can. The
/// usual escape is to erase the future behind a trait object: `dyn Future`.
///
/// That needs two wrappers. `Box` because a trait object is unsized and has
/// to live behind a pointer, and `Pin` because a future may hold references
/// into itself across an `.await`, so once polled it must not move. `Send`
/// because the runner drives these on a multi-threaded runtime.
pub type TestFn =
    fn(TestContext) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaseResult> + Send>>;

/// One case in the catalogue.
pub struct TestCase {
    /// A behaviour sentence, e.g. `schedule_fixed_replicas_across_nodes`.
    pub name: &'static str,
    pub group: TestGroup,
    /// What the cluster must have for this case to mean anything. A case
    /// missing any of these is skipped with a reason, never failed.
    pub requires: &'static [Capability],
    /// The kind of server-owned lease the case's resources live under. A
    /// case that applies jobs needs [`LeaseScope::NodeJobs`]; an
    /// Applications lease refuses them. The group says where the case is
    /// reported, not what it owns.
    pub lease: LeaseScope,
    pub run: TestFn,
}

/// Wrap an ordinary `async fn` into a [`TestFn`].
///
/// Use as `case!(schedule_fixed_replicas_across_nodes)` in a catalogue
/// module. A macro rather than a generic function because the boxing has to
/// happen inside a `fn` item — a closure would capture, and `TestFn` is a
/// plain function pointer precisely so the catalogue stays a `const`-ish
/// list with no allocation per entry.
#[macro_export]
macro_rules! testkit_case {
    ($body:path) => {{
        fn wrapper(
            ctx: $crate::testkit::context::TestContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = $crate::testkit::registry::CaseResult> + Send>,
        > {
            Box::pin(async move {
                $body(ctx)
                    .await
                    .map_err($crate::testkit::registry::CaseError::from)
            })
        }
        wrapper as $crate::testkit::registry::TestFn
    }};
}

/// Every integration case. Chaos scenarios are separate — see
/// [`chaos_cases`] — because they are selected by `--chaos`, not by name.
pub fn all_cases() -> Vec<TestCase> {
    super::cases::all()
}

/// The chaos scenarios.
pub fn chaos_cases() -> Vec<TestCase> {
    super::chaos::scenarios()
}

/// Keep the cases `--filter` names, preserving catalogue order.
///
/// Each comma-separated entry is a group (`scheduling`) or an exact case
/// name (`process_job_with_exit_zero_succeeds`). An empty or
/// whitespace-only filter selects everything, which is what omitting the
/// flag does — so `--filter ""` can't silently run nothing. An entry that is
/// neither is an error naming the valid groups.
pub fn select(cases: Vec<TestCase>, filter: &str) -> Result<Vec<TestCase>, String> {
    let entries: Vec<&str> = filter
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.is_empty() {
        return Ok(cases);
    }
    let mut groups = Vec::new();
    let mut names = Vec::new();
    for entry in entries {
        match entry.parse::<TestGroup>() {
            Ok(group) => groups.push(group),
            Err(_) if cases.iter().any(|case| case.name == entry) => names.push(entry),
            Err(error) => return Err(format!("{error}; or an exact case name")),
        }
    }
    Ok(cases
        .into_iter()
        .filter(|case| groups.contains(&case.group) || names.contains(&case.name))
        .collect())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &'static str, group: TestGroup) -> TestCase {
        async fn body(_ctx: TestContext) -> Result<(), String> {
            Ok(())
        }
        TestCase {
            name,
            group,
            requires: &[],
            lease: LeaseScope::Applications,
            run: crate::testkit_case!(body),
        }
    }

    fn catalogue() -> Vec<TestCase> {
        vec![
            fixture("a", TestGroup::Scheduling),
            fixture("b", TestGroup::Jobs),
            fixture("c", TestGroup::Scheduling),
            fixture("d", TestGroup::Ingress),
        ]
    }

    fn names(filter: &str) -> Vec<&'static str> {
        select(catalogue(), filter)
            .unwrap()
            .iter()
            .map(|case| case.name)
            .collect()
    }

    #[test]
    fn an_empty_filter_selects_every_case() {
        assert_eq!(names(""), vec!["a", "b", "c", "d"]);
        assert_eq!(names("   "), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn a_single_group_selects_its_cases() {
        assert_eq!(names("scheduling"), vec!["a", "c"]);
    }

    #[test]
    fn several_groups_keep_catalogue_order() {
        assert_eq!(names("ingress,scheduling"), vec!["a", "c", "d"]);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(names(" scheduling , jobs "), vec!["a", "b", "c"]);
    }

    /// An exact case name runs just that case, so a fixture can leave out a
    /// sibling it can't support (#742).
    #[test]
    fn an_exact_case_name_selects_only_that_case() {
        assert_eq!(names("c"), vec!["c"]);
    }

    #[test]
    fn groups_and_case_names_combine_in_catalogue_order() {
        assert_eq!(names("d,jobs"), vec!["b", "d"]);
    }

    #[test]
    fn naming_a_case_and_its_group_selects_it_once() {
        assert_eq!(names("a,scheduling"), vec!["a", "c"]);
    }

    #[test]
    fn an_unknown_name_is_rejected_and_names_the_valid_groups() {
        let Err(error) = select(catalogue(), "scheduling,nonsense") else {
            panic!("an unknown name was accepted");
        };
        assert!(error.contains("nonsense"), "{error}");
        assert!(error.contains("scheduling"), "{error}");
        assert!(error.contains("case name"), "{error}");
    }

    #[test]
    fn every_real_case_name_selects_exactly_itself() {
        for case in all_cases() {
            let selected = select(all_cases(), case.name).unwrap();
            assert_eq!(selected.len(), 1, "{}", case.name);
            assert_eq!(selected[0].name, case.name);
        }
    }
}
