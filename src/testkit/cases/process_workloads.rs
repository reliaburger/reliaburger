//! Process-workload cases: apps and jobs that run as host processes.

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::lease::LeaseScope;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A plain process-workload app reaches Running.
async fn process_workload_reports_running(ctx: TestContext) -> Result<(), String> {
    let app = "proc-app";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 1)).await?;
    ctx.wait_running_cluster(app, 1).await
}

/// A job that exits zero ends in the stopped state with a success exit code.
async fn process_job_with_exit_zero_succeeds(ctx: TestContext) -> Result<(), String> {
    let job = "job-ok";
    ctx.apply(&ctx.process_job_spec(job, &["/bin/sh", "-c", "exit 0"]))
        .await?;
    // A run is "stopped" only once its task exited zero; one that failed
    // says "failed".
    ctx.wait_for_job_runs(job, "a run stopped with exit 0", |rows| {
        rows.iter()
            .any(|row| row.instance_id.starts_with("run-") && row.state == "stopped")
    })
    .await
}

/// A job that keeps exiting non-zero eventually reaches the terminal failed
/// state (retry-count is not TOML-configurable, so this rides the run's
/// default attempts).
async fn failing_job_retries_then_fails(ctx: TestContext) -> Result<(), String> {
    let job = "job-fail";
    ctx.apply(&ctx.process_job_spec(job, &["/bin/sh", "-c", "exit 1"]))
        .await?;
    ctx.wait_for_job_runs(job, "a failed run", |rows| {
        rows.iter()
            .any(|row| row.instance_id.starts_with("run-") && row.state == "failed")
    })
    .await
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "process_workload_reports_running",
            group: TestGroup::ProcessWorkloads,
            requires: &[Capability::ProcessRuntime],
            lease: LeaseScope::Applications,
            run: testkit_case!(process_workload_reports_running),
        },
        TestCase {
            name: "process_job_with_exit_zero_succeeds",
            group: TestGroup::ProcessWorkloads,
            requires: &[Capability::ProcessRuntime],
            // An Applications lease refuses jobs (#742).
            lease: LeaseScope::NodeJobs,
            run: testkit_case!(process_job_with_exit_zero_succeeds),
        },
        TestCase {
            name: "failing_job_retries_then_fails",
            group: TestGroup::ProcessWorkloads,
            requires: &[Capability::ProcessRuntime],
            lease: LeaseScope::NodeJobs,
            run: testkit_case!(failing_job_retries_then_fails),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::capabilities::{ClusterCapabilities, StaticCapabilities, WiredSubsystems};
    use crate::relish::client::BunClient;
    use crate::testkit::lease::{LeaseScope, TestLease};
    use crate::testkit::report::{TestOutcome, TestProfile};
    use crate::testkit::runner::{LeaseOwnership, RunConfig};
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{delete, get, post};
    use axum::{Json, Router};
    use std::sync::Arc;
    use std::time::Duration;

    const JOB_LEASE_SUFFIX: &str = "a123456789abcdefa123456789abcdef";

    /// The lease scopes the mock was asked for, in request order.
    type Requested = Arc<tokio::sync::Mutex<Vec<LeaseScope>>>;

    /// Grants what a real server grants: a job lease, in its own
    /// server-chosen namespace, when asked for one, and otherwise an
    /// Applications lease in the requested namespace.
    async fn grant_lease(
        State(requested): State<Requested>,
        Json(request): Json<serde_json::Value>,
    ) -> (StatusCode, Json<TestLease>) {
        let ttl_seconds = request["ttl_seconds"].as_u64().unwrap();
        let lease = if request["scope"] == "node_jobs" {
            requested.lock().await.push(LeaseScope::NodeJobs);
            TestLease::new_scoped(
                format!("node-jobs-{JOB_LEASE_SUFFIX}"),
                "token:runner".into(),
                "runner".into(),
                format!("rbtest-node-{JOB_LEASE_SUFFIX}"),
                1,
                1 + ttl_seconds * 1_000,
                LeaseScope::NodeJobs,
            )
        } else {
            requested.lock().await.push(LeaseScope::Applications);
            TestLease::new(
                "lease-1".into(),
                "token:runner".into(),
                "runner".into(),
                request["namespace"].as_str().unwrap().to_string(),
                1,
                1 + ttl_seconds * 1_000,
            )
        };
        (StatusCode::CREATED, Json(lease.unwrap()))
    }

    /// One run per job case, in the job lease's namespace: `job-ok` exited
    /// zero and `job-fail` ran out of attempts.
    async fn job_runs() -> Json<serde_json::Value> {
        let namespace = format!("rbtest-node-{JOB_LEASE_SUFFIX}");
        let row = |name: &str, state: &str| {
            serde_json::json!({
                "node": "cluster", "name": name, "namespace": namespace,
                "instance_id": "run-1", "image": "app:v1",
                "state": state, "restart_count": 0, "age_seconds": 1,
            })
        };
        Json(serde_json::json!({
            "jobs": [row("job-ok", "stopped"), row("job-fail", "failed")],
            "warnings": [],
        }))
    }

    async fn applied() -> String {
        format!(
            "data: {}\n\n",
            serde_json::to_string(&crate::bun::agent::ApplyEvent::Complete {
                created: 1,
                instances: vec![]
            })
            .unwrap()
        )
    }

    async fn nothing() -> Json<Vec<serde_json::Value>> {
        Json(Vec::new())
    }

    /// A refusal like the one an Applications lease gives a job.
    async fn refused() -> (StatusCode, &'static str) {
        (
            StatusCode::FORBIDDEN,
            "lease scope does not allow this identifier, namespace or resource kind",
        )
    }

    /// A one-node process cluster. With `refuse_jobs` its apply refuses
    /// every job, as an Applications lease does.
    async fn spawn_cluster(refuse_jobs: bool) -> (String, Requested) {
        let requested = Requested::default();
        let apply = if refuse_jobs {
            post(refused)
        } else {
            post(applied)
        };
        let router = Router::new()
            .route("/v1/test/leases", post(grant_lease))
            .route(
                "/v1/test/leases/{id}",
                delete(|| async { StatusCode::NO_CONTENT }),
            )
            .route("/v1/apply", apply)
            .route("/v1/jobs", get(job_runs))
            .route("/v1/cluster/nodes", get(nothing))
            .route("/v1/status", get(nothing))
            .with_state(Arc::clone(&requested));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, requested)
    }

    fn process_cluster() -> ClusterCapabilities {
        ClusterCapabilities::derive(
            &StaticCapabilities {
                container_runtime: "process".to_string(),
                process_workloads: true,
                ..StaticCapabilities::default()
            },
            &WiredSubsystems {
                council: true,
                member_count: Some(1),
                ..WiredSubsystems::default()
            },
        )
    }

    fn run_config(address: &str) -> RunConfig {
        RunConfig {
            client: BunClient::new_with_token(address, None),
            capabilities: process_cluster(),
            run_id: "pw742".to_string(),
            parallel: 1,
            timeout: Duration::from_secs(5),
            chaos: false,
            profile: TestProfile::Development,
            fixed_namespace: None,
            lease_ownership: LeaseOwnership::Required,
            peer_route: crate::testkit::context::PeerRoute::Direct,
        }
    }

    fn job_cases() -> Vec<TestCase> {
        cases()
            .into_iter()
            .filter(|case| {
                matches!(
                    case.name,
                    "process_job_with_exit_zero_succeeds" | "failing_job_retries_then_fails"
                )
            })
            .collect()
    }

    /// #742: the job cases ran under Applications leases, which refuse jobs,
    /// so they failed at apply before any job ran.
    #[tokio::test]
    async fn job_cases_run_under_a_lease_that_permits_jobs() {
        let (address, requested) = spawn_cluster(false).await;

        let report = crate::testkit::runner::run(job_cases(), run_config(&address))
            .await
            .unwrap();

        assert_eq!(report.results.len(), 2);
        for result in &report.results {
            assert_eq!(
                result.outcome,
                TestOutcome::Pass,
                "{} did not pass",
                result.name
            );
        }
        assert_eq!(
            *requested.lock().await,
            vec![LeaseScope::NodeJobs, LeaseScope::NodeJobs]
        );
    }

    /// The app case keeps its Applications lease: only jobs move.
    #[tokio::test]
    async fn the_app_case_keeps_an_applications_lease() {
        let (address, requested) = spawn_cluster(false).await;
        let app_case: Vec<TestCase> = cases()
            .into_iter()
            .filter(|case| case.name == "process_workload_reports_running")
            .collect();
        let mut config = run_config(&address);
        // The mock never reports the app running; only the lease matters.
        config.timeout = Duration::from_millis(300);

        crate::testkit::runner::run(app_case, config).await.unwrap();

        assert_eq!(*requested.lock().await, vec![LeaseScope::Applications]);
    }

    /// A job case can't pass on a cluster that refuses its job: the refusal
    /// is the case's failure, not a skip.
    #[tokio::test]
    async fn job_cases_fail_when_the_cluster_refuses_jobs() {
        let (address, _) = spawn_cluster(true).await;

        let report = crate::testkit::runner::run(job_cases(), run_config(&address))
            .await
            .unwrap();

        assert_eq!(report.results.len(), 2);
        for result in &report.results {
            assert!(
                matches!(
                    &result.outcome,
                    TestOutcome::Fail { reason } if reason.contains("apply failed")
                ),
                "{}: {:?}",
                result.name,
                result.outcome
            );
        }
    }
}
