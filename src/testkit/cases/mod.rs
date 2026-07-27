//! The integration catalogue.
//!
//! One submodule per test group. Each exposes `cases() -> Vec<TestCase>`, and
//! [`all`] concatenates them in the order groups are reported.

use super::registry::TestCase;

mod deployments;
mod health_checks;
mod jobs;
mod process_workloads;
mod scheduling;

/// Every integration case, in the order groups are reported.
pub fn all() -> Vec<TestCase> {
    let mut cases = Vec::new();
    cases.extend(scheduling::cases());
    cases.extend(deployments::cases());
    cases.extend(health_checks::cases());
    cases.extend(process_workloads::cases());
    cases.extend(jobs::cases());
    cases
}

#[cfg(test)]
mod tests {
    use super::super::report::TestGroup;
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn case_names_are_unique() {
        let mut seen = HashSet::new();
        for case in all() {
            assert!(seen.insert(case.name), "duplicate case name: {}", case.name);
        }
    }

    #[test]
    fn every_case_names_a_behaviour_sentence() {
        for case in all() {
            // snake_case behaviour names, per the project convention.
            assert!(
                case.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "case name is not snake_case: {}",
                case.name
            );
            assert!(case.name.contains('_'), "not a sentence: {}", case.name);
        }
    }

    /// This batch (catalogue part A) covers exactly these five groups. The
    /// remaining groups arrive in part B.
    #[test]
    fn part_a_covers_the_expected_groups() {
        let groups: HashSet<TestGroup> = all().iter().map(|case| case.group).collect();
        let expected: HashSet<TestGroup> = [
            TestGroup::Scheduling,
            TestGroup::Deployments,
            TestGroup::HealthChecks,
            TestGroup::ProcessWorkloads,
            TestGroup::Jobs,
        ]
        .into_iter()
        .collect();
        assert_eq!(groups, expected);
    }

    #[test]
    fn every_case_that_deploys_testapp_requires_the_process_runtime() {
        use crate::bun::capabilities::Capability;
        // Only the process runtime can run the `bun testapp` workload, so any
        // case built on it must gate on ProcessRuntime or it would fail rather
        // than skip on a runc cluster.
        for case in all() {
            assert!(
                case.requires.contains(&Capability::ProcessRuntime),
                "{} must require ProcessRuntime",
                case.name
            );
        }
    }

    #[test]
    fn the_catalogue_is_the_expected_size() {
        // Five groups, three cases each.
        assert_eq!(all().len(), 15);
    }
}
