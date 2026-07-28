//! Running a selection of cases and turning it into a report.
//!
//! The runner owns three things a case body must never have to think about:
//! **parallelism** (cases run concurrently, bounded so a big cluster isn't
//! stampeded), **timeouts** (a wedged case fails with a message rather than
//! hanging the run), and **teardown** (every case is cleaned up afterwards, no
//! matter how it ended). Keeping those here is what lets a case body be a plain
//! `async fn` that applies some config and asserts.
//!
//! Nothing here uses `tokio`'s paused clock. Combining `start_paused` with
//! `tokio::spawn` is a standing trap in this codebase — a spawned task can
//! advance the virtual clock out from under the driver — so the runner is
//! timed against the real one, with small durations in tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::bun::capabilities::ClusterCapabilities;
use crate::relish::client::BunClient;

use super::context::TestContext;
use super::deadline::Deadline;
use super::registry::TestCase;
use super::report::{
    CleanupOutcome, EvidenceKind, TestCaseResult, TestEvidence, TestGroup, TestOutcome,
    TestProfile, TestReport, UnknownKind,
};

/// How long teardown gets before the runner records that cleanup is unknown.
/// A hung agent must not wedge the whole run, but lack of cleanup evidence
/// must not disappear behind a green case result either.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything a run needs beyond the cases themselves.
pub struct RunConfig {
    pub client: BunClient,
    pub capabilities: ClusterCapabilities,
    /// Ties every namespace to this invocation. Generated once per
    /// `relish test` call so two concurrent runs can't collide.
    pub run_id: String,
    /// Maximum cases running at once.
    pub parallel: usize,
    /// Per-case budget. A case that overruns it becomes `Unknown(TimedOut)`.
    pub timeout: Duration,
    /// Whether this is the chaos suite (recorded in the report, not behaviour).
    pub chaos: bool,
    /// Determines which known capability gaps may remain optional.
    pub profile: TestProfile,
    /// `--namespace`: run every case in this one namespace instead of a fresh
    /// one each. `None` keeps per-case isolation, which is the default and the
    /// safer choice against a live cluster.
    pub fixed_namespace: Option<String>,
}

/// A finished case tagged with its position in the catalogue, so results can
/// be put back in order after finishing whenever they finished.
struct Indexed {
    index: usize,
    result: TestCaseResult,
}

/// Run `cases` and aggregate a [`TestReport`].
///
/// Cases run concurrently, capped at `config.parallel`. A case missing a
/// capability records a typed skip; a full acceptance profile rejects that
/// skip when the case is required. Each case gets its own namespace (unless a
/// safe `rbtest-*` namespace was explicitly requested) and is torn down
/// afterwards regardless of outcome. Results come back in catalogue order
/// even though cases complete out of order.
pub async fn run(cases: Vec<TestCase>, config: RunConfig) -> TestReport {
    let started_at = now_rfc3339();
    let run_start = Instant::now();
    let cluster_nodes = config.capabilities.node_count;
    let chaos = config.chaos;
    let profile = config.profile;

    // `max(1)` because a semaphore of zero permits would deadlock every case.
    let semaphore = Arc::new(Semaphore::new(config.parallel.max(1)));
    let capabilities = Arc::new(config.capabilities);

    let mut set: JoinSet<Indexed> = JoinSet::new();
    // Task id → identity, so a *panicking* case can still be reported as a
    // failure at its right place instead of sinking the whole run. A panicked
    // task aborts before it can return anything, so its name and index have to
    // be recorded out here.
    let mut identities: HashMap<tokio::task::Id, (usize, String, TestGroup)> = HashMap::new();

    for (index, case) in cases.into_iter().enumerate() {
        let semaphore = Arc::clone(&semaphore);
        let capabilities = Arc::clone(&capabilities);
        let client = config.client.clone();
        let namespace = config
            .fixed_namespace
            .clone()
            .unwrap_or_else(|| TestContext::namespace_for(&config.run_id, index));
        let timeout = config.timeout;
        let profile = config.profile;
        let name = case.name.to_string();
        let group = case.group;

        let handle = set.spawn(async move {
            // Acquire *inside* the task, not before spawning: the semaphore is
            // meant to bound how many run at once, not how many we queue up.
            let _permit = semaphore
                .acquire()
                .await
                .expect("runner semaphore is never closed");
            let result = run_one(
                &case,
                client,
                namespace,
                capabilities.as_ref(),
                timeout,
                profile,
            )
            .await;
            Indexed { index, result }
        });
        identities.insert(handle.id(), (index, name, group));
    }

    let mut collected: Vec<Indexed> = Vec::new();
    while let Some(joined) = set.join_next_with_id().await {
        match joined {
            Ok((_id, indexed)) => collected.push(indexed),
            Err(join_error) => {
                let (index, name, group) = identities.get(&join_error.id()).cloned().unwrap_or((
                    usize::MAX,
                    "<unknown>".to_string(),
                    TestGroup::Scheduling,
                ));
                collected.push(Indexed {
                    index,
                    result: TestCaseResult {
                        name,
                        group,
                        required: true,
                        started_at: now_rfc3339(),
                        finished_at: now_rfc3339(),
                        deadline_at: now_rfc3339(),
                        outcome: TestOutcome::Unknown {
                            kind: UnknownKind::Panicked,
                            reason: format!("runner task panicked: {join_error}"),
                        },
                        duration_ms: 0,
                        evidence: Vec::new(),
                        cleanup: CleanupOutcome::Unknown {
                            reason: "runner task ended before cleanup evidence".to_string(),
                        },
                    },
                });
            }
        }
    }

    collected.sort_by_key(|entry| entry.index);
    let results = collected.into_iter().map(|entry| entry.result).collect();

    TestReport::from_results(
        results,
        started_at,
        run_start.elapsed().as_millis() as u64,
        cluster_nodes,
        chaos,
        profile,
    )
}

/// Run one case: skip-check, timed execution, then unconditional teardown.
async fn run_one(
    case: &TestCase,
    client: BunClient,
    namespace: String,
    capabilities: &ClusterCapabilities,
    timeout: Duration,
    profile: TestProfile,
) -> TestCaseResult {
    let start = Instant::now();
    let started_at = now_rfc3339();
    let deadline_at = rfc3339_after(timeout);
    let required = profile_requires_case(profile, case.requires);

    let missing = capabilities.missing(case.requires);
    if let Some(capability) = missing.first().copied() {
        let names: Vec<String> = missing.iter().map(|c| c.to_string()).collect();
        return TestCaseResult {
            name: case.name.to_string(),
            group: case.group,
            required,
            started_at,
            finished_at: now_rfc3339(),
            deadline_at,
            outcome: TestOutcome::Skipped {
                capability,
                reason: format!("requires {}", names.join(", ")),
            },
            duration_ms: start.elapsed().as_millis() as u64,
            evidence: Vec::new(),
            cleanup: CleanupOutcome::NotRequired,
        };
    }

    let deadline = Deadline::after(timeout).expect("CLI rejects a zero timeout");

    let context = TestContext {
        client,
        namespace,
        capabilities: capabilities.clone(),
        timeout,
        deadline,
    };

    // The case body gets its own task so a panic is data and the outer owner
    // still reaches cleanup. Spawning `run_one` directly did the opposite.
    let body = tokio::spawn((case.run)(context.clone()));
    let outcome = match deadline.run("case", body).await {
        Ok(Ok(Ok(()))) => TestOutcome::Pass,
        Ok(Ok(Err(message))) => match message.strip_prefix(super::registry::UNKNOWN_MARKER) {
            Some(reason) => TestOutcome::Unknown {
                kind: UnknownKind::MissingEvidence,
                reason: format!("case could not establish a verdict: {reason}"),
            },
            None => TestOutcome::Fail { reason: message },
        },
        Ok(Err(join_error)) => TestOutcome::Unknown {
            kind: UnknownKind::Panicked,
            reason: format!("case panicked: {join_error}"),
        },
        Err(_) => TestOutcome::timed_out("case", deadline.budget_ms()),
    };

    let cleanup_deadline = Deadline::after(TEARDOWN_TIMEOUT).expect("non-zero cleanup timeout");
    let cleanup = context.teardown(cleanup_deadline).await;
    let finished_at = now_rfc3339();
    let evidence = matches!(outcome, TestOutcome::Pass)
        .then(|| TestEvidence {
            kind: EvidenceKind::Observed,
            source: case.name.to_string(),
            observed_at: finished_at.clone(),
            detail: "case completed its live assertions".to_string(),
        })
        .into_iter()
        .collect();

    TestCaseResult {
        name: case.name.to_string(),
        group: case.group,
        required,
        started_at,
        finished_at,
        deadline_at,
        outcome,
        duration_ms: start.elapsed().as_millis() as u64,
        evidence,
        cleanup,
    }
}

/// Whether a profile promises to exercise this case rather than merely list it.
fn profile_requires_case(
    profile: TestProfile,
    requires: &[crate::bun::capabilities::Capability],
) -> bool {
    use crate::bun::capabilities::Capability;
    match profile {
        TestProfile::Development => false,
        TestProfile::FullRunc => !requires.contains(&Capability::ProcessRuntime),
        TestProfile::FullApple => !requires.iter().any(|capability| {
            matches!(
                capability,
                Capability::ProcessRuntime
                    | Capability::Ebpf
                    | Capability::Firewall
                    | Capability::CgroupFaults
            )
        }),
        TestProfile::ProcessGrill => !requires.iter().any(|capability| {
            matches!(
                capability,
                Capability::ContainerRuntime
                    | Capability::Ebpf
                    | Capability::Firewall
                    | Capability::Ingress
                    | Capability::CgroupFaults
            )
        }),
    }
}

/// The current UTC time as an RFC 3339 string, e.g. `2026-07-26T18:30:00Z`.
///
/// Built from `OffsetDateTime`'s accessors rather than the `formatting`
/// feature, which the `time` dependency doesn't enable — the accessors are
/// always available, the format string is not.
fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

fn rfc3339_after(duration: Duration) -> String {
    let system_time = std::time::SystemTime::now() + duration;
    let now = time::OffsetDateTime::from(system_time);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::agent::InstanceStatus;
    use crate::bun::capabilities::{Capability, StaticCapabilities, WiredSubsystems};
    use crate::testkit_case;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A cluster with everything wired, so capability gates never skip in a
    /// test that isn't about skipping.
    fn full_capabilities() -> ClusterCapabilities {
        ClusterCapabilities::derive(
            &StaticCapabilities {
                container_runtime: "process".to_string(),
                ebpf: true,
                ingress: true,
                firewall: true,
                identity: true,
                process_workloads: true,
                cgroup_faults: true,
                ..StaticCapabilities::default()
            },
            &WiredSubsystems {
                metrics: true,
                logs: true,
                rollups: true,
                council: true,
                registry: true,
                events: true,
                member_count: Some(3),
                ..WiredSubsystems::default()
            },
        )
    }

    fn config(client: BunClient, capabilities: ClusterCapabilities, parallel: usize) -> RunConfig {
        RunConfig {
            client,
            capabilities,
            run_id: "unit".to_string(),
            parallel,
            timeout: Duration::from_secs(5),
            chaos: false,
            profile: TestProfile::Development,
            fixed_namespace: None,
        }
    }

    /// A client pointed nowhere. Teardown against it fails fast and is
    /// swallowed, so cases that don't care about teardown can use it.
    fn dead_client() -> BunClient {
        BunClient::new_with_token("http://127.0.0.1:1", None)
    }

    fn case(
        name: &'static str,
        requires: &'static [Capability],
        run: super::super::registry::TestFn,
    ) -> TestCase {
        TestCase {
            name,
            group: TestGroup::Scheduling,
            requires,
            run,
        }
    }

    static SKIP_BODY_RAN: AtomicBool = AtomicBool::new(false);

    #[tokio::test]
    async fn a_missing_capability_skips_the_case_without_running_it() {
        SKIP_BODY_RAN.store(false, Ordering::SeqCst);
        async fn body(_ctx: TestContext) -> Result<(), String> {
            SKIP_BODY_RAN.store(true, Ordering::SeqCst);
            Ok(())
        }
        // Capabilities with everything *except* eBPF; the case needs eBPF.
        let mut caps = full_capabilities();
        caps.ebpf = false;
        let cases = vec![case("needs_ebpf", &[Capability::Ebpf], testkit_case!(body))];

        let report = run(cases, config(dead_client(), caps, 4)).await;

        assert_eq!(report.skipped, 1);
        assert_eq!(report.passed, 0);
        assert_eq!(report.failed, 0);
        assert!(
            !SKIP_BODY_RAN.load(Ordering::SeqCst),
            "a skipped case must not have run its body"
        );
        match &report.results[0].outcome {
            TestOutcome::Skipped { reason, capability } => {
                assert_eq!(*capability, Capability::Ebpf);
                assert!(reason.contains("ebpf"), "{reason}");
            }
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_passing_case_is_passed_and_a_failing_case_carries_its_message() {
        async fn passes(_ctx: TestContext) -> Result<(), String> {
            Ok(())
        }
        async fn fails(_ctx: TestContext) -> Result<(), String> {
            Err("expected 3, saw 1".to_string())
        }
        let cases = vec![
            case("passes", &[], testkit_case!(passes)),
            case("fails", &[], testkit_case!(fails)),
        ];

        let report = run(cases, config(dead_client(), full_capabilities(), 4)).await;

        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.results[0].outcome, TestOutcome::Pass);
        match &report.results[1].outcome {
            TestOutcome::Fail { reason } => assert_eq!(reason, "expected 3, saw 1"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_case_without_runtime_evidence_becomes_unknown() {
        async fn body(_ctx: TestContext) -> Result<(), String> {
            super::super::registry::unknown("no labelled node to target")
        }
        let cases = vec![case("missing_evidence", &[], testkit_case!(body))];

        let report = run(cases, config(dead_client(), full_capabilities(), 4)).await;

        assert_eq!(report.unknown, 1);
        assert_eq!(report.failed, 0);
        match &report.results[0].outcome {
            TestOutcome::Unknown { kind, reason } => {
                assert_eq!(*kind, UnknownKind::MissingEvidence);
                assert!(reason.contains("no labelled node to target"));
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_case_that_overruns_its_timeout_is_timed_out() {
        async fn slow(_ctx: TestContext) -> Result<(), String> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
        let cases = vec![case("slow", &[], testkit_case!(slow))];
        let mut cfg = config(dead_client(), full_capabilities(), 4);
        cfg.timeout = Duration::from_millis(50);

        let report = run(cases, cfg).await;

        assert_eq!(
            report.failed, 0,
            "unknown is counted separately from failure"
        );
        assert_eq!(report.unknown, 1);
        assert!(matches!(
            report.results[0].outcome,
            TestOutcome::Unknown {
                kind: UnknownKind::TimedOut,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_panicking_case_becomes_unknown_and_still_reaches_cleanup() {
        async fn boom(_ctx: TestContext) -> Result<(), String> {
            panic!("something unexpected");
        }
        async fn fine(_ctx: TestContext) -> Result<(), String> {
            Ok(())
        }
        let cases = vec![
            case("boom", &[], testkit_case!(boom)),
            case("fine", &[], testkit_case!(fine)),
        ];

        let report = run(cases, config(dead_client(), full_capabilities(), 4)).await;

        assert_eq!(report.total, 2);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.unknown, 1);
        // The panicked case keeps its identity and its place.
        assert_eq!(report.results[0].name, "boom");
        assert!(matches!(
            report.results[0].outcome,
            TestOutcome::Unknown {
                kind: UnknownKind::Panicked,
                ..
            }
        ));
        assert_eq!(report.results[1].name, "fine");
    }

    static ORDER_TAIL_RAN: AtomicBool = AtomicBool::new(false);

    #[tokio::test]
    async fn results_come_back_in_catalogue_order_despite_finishing_out_of_order() {
        // `first` sleeps longest, so it finishes last; `third` finishes first.
        async fn first(_ctx: TestContext) -> Result<(), String> {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(())
        }
        async fn second(_ctx: TestContext) -> Result<(), String> {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(())
        }
        async fn third(_ctx: TestContext) -> Result<(), String> {
            ORDER_TAIL_RAN.store(true, Ordering::SeqCst);
            Ok(())
        }
        let cases = vec![
            case("first", &[], testkit_case!(first)),
            case("second", &[], testkit_case!(second)),
            case("third", &[], testkit_case!(third)),
        ];

        let report = run(cases, config(dead_client(), full_capabilities(), 4)).await;

        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
        assert!(ORDER_TAIL_RAN.load(Ordering::SeqCst));
    }

    static CONC_CURRENT: AtomicUsize = AtomicUsize::new(0);
    static CONC_MAX: AtomicUsize = AtomicUsize::new(0);

    #[tokio::test]
    async fn concurrency_never_exceeds_the_parallel_limit() {
        CONC_CURRENT.store(0, Ordering::SeqCst);
        CONC_MAX.store(0, Ordering::SeqCst);
        async fn body(_ctx: TestContext) -> Result<(), String> {
            let now = CONC_CURRENT.fetch_add(1, Ordering::SeqCst) + 1;
            CONC_MAX.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            CONC_CURRENT.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
        let cases: Vec<TestCase> = (0..6)
            .map(|_| case("body", &[], testkit_case!(body)))
            .collect();

        let report = run(cases, config(dead_client(), full_capabilities(), 2)).await;

        assert_eq!(report.passed, 6);
        let max = CONC_MAX.load(Ordering::SeqCst);
        assert!(max <= 2, "ran {max} at once with a limit of 2");
        assert!(max >= 2, "the two-wide limit was never actually reached");
    }

    /// A minimal HTTP/1.1 mock that answers `GET /v1/status` with a fixed
    /// instance list and records every `POST /v1/stop/...` path. Enough to
    /// prove the runner tears down; not a general server.
    async fn spawn_stop_recorder(
        status_body: String,
    ) -> (String, Arc<tokio::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let recorder = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let recorder_for_task = Arc::clone(&recorder);
        let remaining: Vec<InstanceStatus> = serde_json::from_str(&status_body).unwrap();
        let remaining = Arc::new(tokio::sync::Mutex::new(remaining));
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let recorder = Arc::clone(&recorder_for_task);
                let remaining = Arc::clone(&remaining);
                tokio::spawn(handle_mock_conn(socket, recorder, remaining));
            }
        });
        (address, recorder)
    }

    async fn handle_mock_conn(
        mut socket: TcpStream,
        recorder: Arc<tokio::sync::Mutex<Vec<String>>>,
        remaining: Arc<tokio::sync::Mutex<Vec<InstanceStatus>>>,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
        let request = String::from_utf8_lossy(&buf);
        let request_line = request.lines().next().unwrap_or("");
        let response = if request_line.starts_with("GET /v1/status") {
            let status_body = serde_json::to_string(&*remaining.lock().await).unwrap();
            mock_response(200, &status_body)
        } else if request_line.starts_with("POST /v1/stop/") {
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            recorder.lock().await.push(path.clone());
            if let Some(target) = path.strip_prefix("/v1/stop/")
                && let Some((app, namespace)) = target.split_once('/')
            {
                remaining
                    .lock()
                    .await
                    .retain(|instance| instance.app_name != app || instance.namespace != namespace);
            }
            mock_response(200, "")
        } else {
            mock_response(404, "")
        };
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    }

    fn mock_response(status: u16, body: &str) -> String {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn instance(app: &str, namespace: &str) -> InstanceStatus {
        InstanceStatus {
            id: format!("{app}-0"),
            app_name: app.to_string(),
            namespace: namespace.to_string(),
            state: "running".to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid: None,
        }
    }

    #[tokio::test]
    async fn teardown_runs_after_pass_fail_and_timeout() {
        // One leftover app per case namespace. run_id "td" + indices 0..2 →
        // rbtest-td-00 / -01 / -02.
        let namespaces = ["rbtest-td-00", "rbtest-td-01", "rbtest-td-02"];
        let instances: Vec<InstanceStatus> = namespaces
            .iter()
            .map(|ns| instance("leftover", ns))
            .collect();
        let status_body = serde_json::to_string(&instances).unwrap();
        let (address, recorder) = spawn_stop_recorder(status_body).await;
        let client = BunClient::new_with_token(&address, None);

        async fn passes(_ctx: TestContext) -> Result<(), String> {
            Ok(())
        }
        async fn fails(_ctx: TestContext) -> Result<(), String> {
            Err("nope".to_string())
        }
        async fn slow(_ctx: TestContext) -> Result<(), String> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
        let cases = vec![
            case("passes", &[], testkit_case!(passes)),
            case("fails", &[], testkit_case!(fails)),
            case("slow", &[], testkit_case!(slow)),
        ];

        let mut cfg = config(client, full_capabilities(), 4);
        cfg.run_id = "td".to_string();
        cfg.timeout = Duration::from_millis(50);
        let report = run(cases, cfg).await;

        assert_eq!(report.total, 3);
        // Give any trailing teardown connections a moment to be recorded.
        let stops = recorder.lock().await.clone();
        let mut stopped: Vec<String> = stops;
        stopped.sort();
        assert_eq!(
            stopped,
            vec![
                "/v1/stop/leftover/rbtest-td-00".to_string(),
                "/v1/stop/leftover/rbtest-td-01".to_string(),
                "/v1/stop/leftover/rbtest-td-02".to_string(),
            ],
            "every case — pass, fail and timeout — must be torn down"
        );
    }

    #[test]
    fn now_rfc3339_looks_like_a_timestamp() {
        let stamp = now_rfc3339();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(&stamp[4..5], "-", "{stamp}");
        assert_eq!(&stamp[10..11], "T", "{stamp}");
        // Parses back as an RFC 3339 instant if the feature is on; at minimum
        // the year is four digits.
        assert!(stamp[..4].chars().all(|c| c.is_ascii_digit()), "{stamp}");
    }
}
