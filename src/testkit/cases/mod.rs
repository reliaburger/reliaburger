//! The integration catalogue.
//!
//! One submodule per test group. Each exposes `cases() -> Vec<TestCase>`, and
//! [`all`] concatenates them in the order groups are reported.

use super::registry::TestCase;

mod cluster_coordination;
mod deployments;
mod firewall;
mod health_checks;
mod image_registry;
mod ingress;
mod jobs;
mod process_workloads;
mod scheduling;
mod secrets_config;
mod service_discovery;
mod volumes;
mod workload_identity;

/// Every integration case, in the order groups are reported.
pub fn all() -> Vec<TestCase> {
    let mut cases = Vec::new();
    cases.extend(scheduling::cases());
    cases.extend(service_discovery::cases());
    cases.extend(deployments::cases());
    cases.extend(health_checks::cases());
    cases.extend(secrets_config::cases());
    cases.extend(firewall::cases());
    cases.extend(workload_identity::cases());
    cases.extend(ingress::cases());
    cases.extend(volumes::cases());
    cases.extend(process_workloads::cases());
    cases.extend(jobs::cases());
    cases.extend(image_registry::cases());
    cases.extend(cluster_coordination::cases());
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

    /// The whole catalogue: every one of the thirteen groups is covered.
    #[test]
    fn the_expected_groups_are_covered() {
        let groups: HashSet<TestGroup> = all().iter().map(|case| case.group).collect();
        let expected: HashSet<TestGroup> = [
            TestGroup::Scheduling,
            TestGroup::ServiceDiscovery,
            TestGroup::Deployments,
            TestGroup::HealthChecks,
            TestGroup::SecretsConfig,
            TestGroup::Firewall,
            TestGroup::WorkloadIdentity,
            TestGroup::Ingress,
            TestGroup::Volumes,
            TestGroup::ProcessWorkloads,
            TestGroup::Jobs,
            TestGroup::ImageRegistry,
            TestGroup::ClusterCoordination,
        ]
        .into_iter()
        .collect();
        assert_eq!(groups, expected);
    }

    /// Every case that deploys the `testapp` workload gates on ProcessRuntime,
    /// or it would fail rather than skip on a runc cluster. Control-plane
    /// groups (identity, cluster-coordination) deploy nothing, so they're
    /// exempt.
    #[test]
    fn every_workload_case_requires_the_process_runtime() {
        use crate::bun::capabilities::Capability;
        let workload_groups = [
            TestGroup::Scheduling,
            TestGroup::ServiceDiscovery,
            TestGroup::Deployments,
            TestGroup::HealthChecks,
            TestGroup::ProcessWorkloads,
            TestGroup::Jobs,
        ];
        for case in all() {
            if workload_groups.contains(&case.group) {
                assert!(
                    case.requires.contains(&Capability::ProcessRuntime),
                    "{} deploys a workload and must require ProcessRuntime",
                    case.name
                );
            }
        }
    }

    #[test]
    fn the_catalogue_is_the_expected_size() {
        // All thirteen groups, three cases each.
        assert_eq!(all().len(), 39);
    }
}
