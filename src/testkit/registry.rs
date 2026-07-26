//! The catalogue of test cases, and how `--filter` selects from it.

use crate::bun::capabilities::Capability;

use super::context::TestContext;
use super::report::TestGroup;

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
    fn(
        TestContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// One case in the catalogue.
pub struct TestCase {
    /// A behaviour sentence, e.g. `schedule_fixed_replicas_across_nodes`.
    pub name: &'static str,
    pub group: TestGroup,
    /// What the cluster must have for this case to mean anything. A case
    /// missing any of these is skipped with a reason, never failed.
    pub requires: &'static [Capability],
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
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        {
            Box::pin($body(ctx))
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

/// Parse `--filter scheduling,firewall` into groups.
///
/// An empty or whitespace-only filter selects everything, which is what
/// omitting the flag does — so `--filter ""` can't silently run nothing.
pub fn parse_filter(filter: &str) -> Result<Vec<TestGroup>, String> {
    let trimmed = filter.trim();
    if trimmed.is_empty() {
        return Ok(TestGroup::ALL.to_vec());
    }
    trimmed
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<TestGroup>())
        .collect()
}

/// Keep only the cases in `groups`, preserving catalogue order.
pub fn select(cases: Vec<TestCase>, groups: &[TestGroup]) -> Vec<TestCase> {
    cases
        .into_iter()
        .filter(|case| groups.contains(&case.group))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_filter_selects_every_group() {
        assert_eq!(parse_filter("").unwrap(), TestGroup::ALL.to_vec());
        assert_eq!(parse_filter("   ").unwrap(), TestGroup::ALL.to_vec());
    }

    #[test]
    fn a_single_group_parses() {
        assert_eq!(
            parse_filter("scheduling").unwrap(),
            vec![TestGroup::Scheduling]
        );
    }

    #[test]
    fn several_comma_separated_groups_parse() {
        assert_eq!(
            parse_filter("scheduling,firewall,jobs").unwrap(),
            vec![TestGroup::Scheduling, TestGroup::Firewall, TestGroup::Jobs]
        );
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            parse_filter(" scheduling , jobs ").unwrap(),
            vec![TestGroup::Scheduling, TestGroup::Jobs]
        );
    }

    #[test]
    fn an_unknown_group_is_rejected_and_names_the_valid_ones() {
        let error = parse_filter("scheduling,nonsense").unwrap_err();
        assert!(error.contains("nonsense"), "{error}");
        assert!(error.contains("scheduling"), "{error}");
    }

    fn fixture(name: &'static str, group: TestGroup) -> TestCase {
        async fn body(_ctx: TestContext) -> Result<(), String> {
            Ok(())
        }
        TestCase {
            name,
            group,
            requires: &[],
            run: crate::testkit_case!(body),
        }
    }

    #[test]
    fn select_keeps_only_the_named_groups_in_catalogue_order() {
        let cases = vec![
            fixture("a", TestGroup::Scheduling),
            fixture("b", TestGroup::Jobs),
            fixture("c", TestGroup::Scheduling),
            fixture("d", TestGroup::Ingress),
        ];
        let selected = select(cases, &[TestGroup::Scheduling, TestGroup::Ingress]);
        let names: Vec<&str> = selected.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["a", "c", "d"]);
    }

    #[test]
    fn selecting_no_groups_yields_no_cases() {
        let cases = vec![fixture("a", TestGroup::Scheduling)];
        assert!(select(cases, &[]).is_empty());
    }
}
