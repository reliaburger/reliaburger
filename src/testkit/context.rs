//! What a test case is handed.
//!
//! The context is the only way a case touches the cluster: a `BunClient`
//! pointed at a node, and a namespace of its own. Everything a case creates
//! carries that namespace, and teardown stops that namespace and nothing
//! else — which is what makes it safe to point `relish test` at a cluster
//! that has real work on it.

use std::time::Duration;

use crate::bun::capabilities::ClusterCapabilities;
use crate::relish::client::BunClient;

/// Prefix for every namespace the test runner creates.
///
/// The safety net for running against a real cluster: teardown only ever
/// touches namespaces it made, and they are recognisable at a glance in
/// `relish status` if something does leak.
pub const TEST_NAMESPACE_PREFIX: &str = "rbtest";

/// One test case's handle on the cluster.
#[derive(Clone)]
pub struct TestContext {
    pub client: BunClient,
    /// This case's own namespace, e.g. `rbtest-4f2a91-03`.
    pub namespace: String,
    pub capabilities: ClusterCapabilities,
    /// Per-case budget, from `--timeout`. Poll loops measure against it so a
    /// case fails with a useful message rather than being killed from
    /// outside with none.
    pub timeout: Duration,
}

impl TestContext {
    /// Build a namespace name for case number `seq` of run `run_id`.
    pub fn namespace_for(run_id: &str, seq: usize) -> String {
        format!("{TEST_NAMESPACE_PREFIX}-{run_id}-{seq:02}")
    }

    /// Whether `namespace` was created by the test runner.
    ///
    /// Teardown consults this before stopping anything. A bug that pointed
    /// teardown at an operator's namespace would be the worst thing this
    /// tool could do, so the check is explicit rather than implied by how
    /// the name was built.
    pub fn is_test_namespace(namespace: &str) -> bool {
        namespace
            .strip_prefix(TEST_NAMESPACE_PREFIX)
            .is_some_and(|rest| rest.starts_with('-'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_are_unique_per_case_and_run() {
        let a = TestContext::namespace_for("4f2a91", 3);
        let b = TestContext::namespace_for("4f2a91", 4);
        let c = TestContext::namespace_for("99bb00", 3);
        assert_eq!(a, "rbtest-4f2a91-03");
        assert_ne!(a, b, "two cases in one run must not share a namespace");
        assert_ne!(a, c, "two runs must not share a namespace");
    }

    /// Teardown keys off this. A false positive here would let the runner
    /// stop an operator's apps, which is the worst thing this tool could do.
    #[test]
    fn only_runner_created_namespaces_are_recognised() {
        assert!(TestContext::is_test_namespace("rbtest-4f2a91-03"));
        assert!(TestContext::is_test_namespace("rbtest-anything"));

        assert!(!TestContext::is_test_namespace("default"));
        assert!(!TestContext::is_test_namespace("production"));
        // A namespace that merely starts with the letters must not match —
        // `rbtestingground` is somebody's real namespace.
        assert!(!TestContext::is_test_namespace("rbtestingground"));
        assert!(!TestContext::is_test_namespace("rbtest"));
        assert!(!TestContext::is_test_namespace("my-rbtest-01"));
    }
}
