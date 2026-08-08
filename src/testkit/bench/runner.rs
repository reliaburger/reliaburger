//! Bounded orchestration and unconditional cleanup for `relish bench`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::bun::capabilities::ClusterCapabilities;
use crate::relish::client::BunClient;
use crate::testkit::chaos::ChaosGuard;
use crate::testkit::deadline::Deadline;

use super::environment::{build_environment, detect_hosted_environment};
use super::report::{BENCH_SCHEMA_VERSION, BenchReport, FailedSuite, SkippedSuite};
use super::suites::{
    BenchLeaseOwner, Preflight, SuiteContext, SuiteKind, SuiteOptions, execute, preflight,
};

const QUICK_TIMEOUT: Duration = Duration::from_secs(60);
const FULL_TIMEOUT: Duration = Duration::from_secs(300);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Operator-selected benchmark shape.
#[derive(Debug, Clone, Copy)]
pub struct BenchRunOptions {
    /// Use the smaller sample and workload sizes.
    pub quick: bool,
    /// Include the deliberately saturating cluster-capacity probe.
    pub capacity: bool,
    /// Permit the leader-reconstruction probe to fail a real node.
    pub disruptive: bool,
    /// Acknowledge capacity saturation and disruptive actions.
    pub yes: bool,
}

/// The harness itself could not establish trustworthy run-level evidence.
#[derive(Debug, thiserror::Error)]
pub enum BenchRunnerError {
    /// The entry node did not return local fingerprints and policy.
    #[error("could not read local capability evidence: {0}")]
    LocalCapabilities(String),
    /// The bounded cross-node fingerprint collection failed.
    #[error("could not collect cluster capability evidence: {0}")]
    ClusterCapabilities(String),
    /// A council-enabled node did not return its voter topology.
    #[error("could not inspect council topology: {0}")]
    Council(String),
}

/// Run every suite in catalogue order.
pub async fn run(
    client: BunClient,
    options: BenchRunOptions,
) -> Result<BenchReport, BenchRunnerError> {
    let started_at = crate::testkit::runner::now_rfc3339();
    let started = Instant::now();
    let initial = client
        .capabilities()
        .await
        .map_err(|error| BenchRunnerError::LocalCapabilities(error.to_string()))?;
    let cluster = client
        .cluster_capabilities()
        .await
        .map_err(|error| BenchRunnerError::ClusterCapabilities(error.to_string()))?;
    let council_members = if initial.council {
        client
            .council()
            .await
            .map_err(|error| BenchRunnerError::Council(error.to_string()))?
            .members
            .len() as u32
    } else {
        0
    };
    let hosted = detect_hosted_environment(&host_environment());
    let environment = build_environment(&initial, &cluster, council_members, hosted);
    let timeout = if options.quick {
        QUICK_TIMEOUT
    } else {
        FULL_TIMEOUT
    };
    let suite_options = SuiteOptions {
        quick: options.quick,
        capacity: options.capacity,
        disruptive: options.disruptive,
        yes: options.yes,
    };
    let mut metrics = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for (index, suite) in SuiteKind::ALL.into_iter().enumerate() {
        let name = suite.name().to_string();
        let capabilities = match refresh_capabilities(&client).await {
            Ok(capabilities) => capabilities,
            Err(reason) => {
                failed.push(FailedSuite { name, reason });
                continue;
            }
        };
        match preflight(suite, suite_options, &capabilities) {
            Ok(()) => {}
            Err(Preflight::Skip(reason)) => {
                skipped.push(SkippedSuite { name, reason });
                continue;
            }
            Err(Preflight::Fail(reason)) => {
                failed.push(FailedSuite { name, reason });
                continue;
            }
        }

        let ttl_seconds = timeout
            .saturating_add(CLEANUP_TIMEOUT)
            .as_secs()
            .saturating_add(1);
        if ttl_seconds > capabilities.test_policy.max_lease_seconds {
            failed.push(FailedSuite {
                name,
                reason: format!(
                    "suite and cleanup need a {ttl_seconds}s lease, but server policy permits at most {}s",
                    capabilities.test_policy.max_lease_seconds
                ),
            });
            continue;
        }
        let namespace = benchmark_namespace(started, index);
        let leases = BenchLeaseOwner::new(client.clone(), ttl_seconds);
        let lease = match tokio::time::timeout(Duration::from_secs(10), leases.create(&namespace))
            .await
        {
            Ok(Ok(lease)) => lease,
            Ok(Err(reason)) => {
                failed.push(FailedSuite { name, reason });
                continue;
            }
            Err(_) => {
                failed.push(FailedSuite {
                    name,
                    reason: "lease creation timed out; the server TTL reaper owns any late lease"
                        .to_string(),
                });
                continue;
            }
        };
        let chaos = ChaosGuard::default();
        let deadline = Deadline::after(timeout).expect("suite timeout is non-zero");
        let context = SuiteContext {
            client: client.clone(),
            capabilities,
            namespace: lease.namespace,
            lease_id: lease.lease_id,
            leases: leases.clone(),
            chaos: chaos.clone(),
            timeout,
            deadline,
            options: suite_options,
        };
        let execution = bounded_execution(timeout, execute(suite, context)).await;

        let fault_cleanup = chaos
            .cleanup(Deadline::after(CLEANUP_TIMEOUT).expect("cleanup is non-zero"))
            .await;
        let lease_cleanup = leases
            .cleanup(Deadline::after(CLEANUP_TIMEOUT).expect("cleanup is non-zero"))
            .await;
        let cleanup_error = cleanup_failure(fault_cleanup, lease_cleanup.err());
        match (execution, cleanup_error) {
            (Ok(metric), None) => metrics.push(metric),
            (Ok(_), Some(reason)) | (Err(reason), None) => {
                failed.push(FailedSuite { name, reason });
            }
            (Err(reason), Some(cleanup)) => failed.push(FailedSuite {
                name,
                reason: format!("{reason}; {cleanup}"),
            }),
        }
    }

    Ok(BenchReport {
        schema_version: BENCH_SCHEMA_VERSION,
        started_at,
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        quick: options.quick,
        capacity: options.capacity,
        environment,
        metrics,
        skipped,
        failed,
    })
}

async fn bounded_execution<T, F>(timeout: Duration, future: F) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    let mut task = tokio::spawn(future);
    match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => Err(format!("suite task panicked: {join_error}")),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(format!("timed out after {} ms", timeout.as_millis()))
        }
    }
}

async fn refresh_capabilities(client: &BunClient) -> Result<ClusterCapabilities, String> {
    match tokio::time::timeout(Duration::from_secs(5), client.capabilities()).await {
        Ok(Ok(capabilities)) => Ok(capabilities),
        Ok(Err(error)) => Err(format!("capability refresh failed: {error}")),
        Err(_) => Err("capability refresh timed out".to_string()),
    }
}

fn cleanup_failure(
    faults: crate::testkit::CleanupOutcome,
    leases: Option<String>,
) -> Option<String> {
    let faults = match faults {
        crate::testkit::CleanupOutcome::Confirmed | crate::testkit::CleanupOutcome::NotRequired => {
            None
        }
        crate::testkit::CleanupOutcome::Failed { reason }
        | crate::testkit::CleanupOutcome::Unknown { reason } => Some(reason),
    };
    match (faults, leases) {
        (None, None) => None,
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (Some(faults), Some(leases)) => Some(format!("{faults}; {leases}")),
    }
}

fn benchmark_namespace(started: Instant, index: usize) -> String {
    let tag = started.elapsed().as_nanos() as u64
        ^ std::process::id() as u64
        ^ index as u64
        ^ crate::testkit::lease::now_unix_millis();
    format!("rbtest-bench-{:08x}-{index}", tag & 0xffff_ffff)
}

fn host_environment() -> BTreeMap<String, String> {
    ["GITHUB_ACTIONS", "GITLAB_CI", "BUILDKITE", "CI"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_failure_never_hides_unknown_ownership() {
        assert_eq!(
            cleanup_failure(
                crate::testkit::CleanupOutcome::Unknown {
                    reason: "fault state unknown".to_string(),
                },
                Some("lease state unknown".to_string()),
            ),
            Some("fault state unknown; lease state unknown".to_string())
        );
    }

    #[test]
    fn benchmark_namespaces_are_reserved_dns_labels() {
        let namespace = benchmark_namespace(Instant::now(), 6);
        assert!(crate::testkit::lease::valid_test_namespace(&namespace));
    }

    #[tokio::test]
    async fn a_started_suite_timeout_is_a_failure_not_a_skip() {
        let result = bounded_execution(Duration::from_millis(1), async {
            std::future::pending::<Result<(), String>>().await
        })
        .await;

        assert_eq!(result, Err("timed out after 1 ms".to_string()));
    }
}
