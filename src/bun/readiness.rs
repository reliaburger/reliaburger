//! Live evidence for Bun's long-lived subsystems.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Lifecycle state of a long-lived Bun subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemState {
    /// The owner is being constructed or restarted.
    Starting,
    /// The owner has acquired its resources and entered its run loop.
    Ready,
    /// The owner failed while the Bun process remains alive.
    Degraded,
    /// The owner stopped during shutdown or exhausted its restart budget.
    Stopped,
}

/// Operator-visible evidence for one long-lived subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemEvidence {
    /// Stable subsystem name.
    pub name: String,
    /// Whether losing this subsystem fences new scheduling on the node.
    pub critical: bool,
    /// Current lifecycle state.
    pub state: SubsystemState,
    /// Wall-clock time of the latest state transition.
    pub state_since_unix_ms: u64,
    /// Latest failure, retained across a successful restart for diagnosis.
    pub last_error: Option<String>,
    /// Wall-clock time of the latest failure.
    pub last_error_unix_ms: Option<u64>,
    /// Number of reconstruction attempts after the initial run.
    pub restart_count: u32,
}

/// A point-in-time node readiness decision and its supporting evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReadinessEvidence {
    /// True only when at least one critical subsystem exists and all critical
    /// subsystems are ready.
    pub ready: bool,
    /// Time this evidence snapshot was assembled.
    pub observed_at_unix_ms: u64,
    /// Stable name-ordered subsystem evidence.
    pub subsystems: Vec<SubsystemEvidence>,
}

/// HTTP and Phase 15 view of live placement capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilityEvidence {
    /// Whether the complete critical subsystem set is ready.
    pub ready: bool,
    /// Kernel enforcement hooks and runtime pre-start support.
    pub egress: crate::sesame::egress::EgressEnforcementCapability,
    /// Resolver transports and workload reachability.
    pub dns: crate::onion::dns::DnsCapability,
    /// Time this evidence snapshot was assembled.
    pub observed_at_unix_ms: u64,
}

#[derive(Default)]
struct ReadinessInner {
    subsystems: BTreeMap<String, SubsystemEvidence>,
    capabilities: crate::meat::cluster_state::NodeCapabilities,
}

/// Shared live evidence store for the Bun process.
#[derive(Clone, Default)]
pub struct ReadinessTracker {
    inner: Arc<RwLock<ReadinessInner>>,
}

impl ReadinessTracker {
    /// Create an empty tracker. An empty tracker is not ready.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a subsystem in `Starting` state.
    pub async fn register(&self, name: impl Into<String>, critical: bool) {
        let name = name.into();
        let now = unix_millis(SystemTime::now());
        self.inner
            .write()
            .await
            .subsystems
            .entry(name.clone())
            .and_modify(|entry| {
                entry.critical = critical;
                entry.state = SubsystemState::Starting;
                entry.state_since_unix_ms = now;
            })
            .or_insert(SubsystemEvidence {
                name,
                critical,
                state: SubsystemState::Starting,
                state_since_unix_ms: now,
                last_error: None,
                last_error_unix_ms: None,
                restart_count: 0,
            });
    }

    /// Mark a registered subsystem ready.
    pub async fn ready(&self, name: &str) {
        self.transition(name, SubsystemState::Ready, None).await;
    }

    /// Mark a registered subsystem degraded and retain the failure evidence.
    pub async fn degraded(&self, name: &str, error: impl Into<String>) {
        self.transition(name, SubsystemState::Degraded, Some(error.into()))
            .await;
    }

    /// Mark a registered subsystem stopped, optionally recording a failure.
    pub async fn stopped(&self, name: &str, error: Option<String>) {
        self.transition(name, SubsystemState::Stopped, error).await;
    }

    async fn record_restart(&self, name: &str) -> u32 {
        let mut inner = self.inner.write().await;
        let Some(entry) = inner.subsystems.get_mut(name) else {
            return 0;
        };
        entry.restart_count = entry.restart_count.saturating_add(1);
        entry.restart_count
    }

    async fn transition(&self, name: &str, state: SubsystemState, error: Option<String>) {
        let now = unix_millis(SystemTime::now());
        let mut inner = self.inner.write().await;
        let Some(entry) = inner.subsystems.get_mut(name) else {
            return;
        };
        entry.state = state;
        entry.state_since_unix_ms = now;
        if let Some(error) = error {
            entry.last_error = Some(error);
            entry.last_error_unix_ms = Some(now);
        }
    }

    /// Replace the node's latest live placement capabilities.
    pub async fn set_capabilities(
        &self,
        capabilities: crate::meat::cluster_state::NodeCapabilities,
    ) {
        self.inner.write().await.capabilities = capabilities;
    }

    /// Snapshot readiness without holding the tracker lock across callers.
    pub async fn snapshot(&self) -> NodeReadinessEvidence {
        self.snapshots().await.0
    }

    /// Snapshot readiness and placement facts under one read lock and clock.
    pub async fn snapshots(&self) -> (NodeReadinessEvidence, NodeCapabilityEvidence) {
        let inner = self.inner.read().await;
        let subsystems: Vec<_> = inner.subsystems.values().cloned().collect();
        let critical_count = subsystems.iter().filter(|entry| entry.critical).count();
        let ready = critical_count > 0
            && subsystems
                .iter()
                .filter(|entry| entry.critical)
                .all(|entry| entry.state == SubsystemState::Ready);
        let observed_at_unix_ms = unix_millis(SystemTime::now());
        let readiness = NodeReadinessEvidence {
            ready,
            observed_at_unix_ms,
            subsystems,
        };
        let placement = NodeCapabilityEvidence {
            ready,
            egress: inner.capabilities.egress,
            dns: inner.capabilities.dns,
            observed_at_unix_ms,
        };
        (readiness, placement)
    }

    /// Snapshot capabilities beside the same critical readiness decision.
    pub async fn capability_snapshot(&self) -> NodeCapabilityEvidence {
        self.snapshots().await.1
    }
}

/// A bounded restart policy for a task whose factory can reconstruct every
/// resource it owns. Tasks owning unique sockets or one-shot channel receivers
/// must use [`spawn_owned`] instead.
#[derive(Debug, Clone, Copy)]
pub struct RestartBudget {
    /// Maximum reconstruction attempts after the initial run.
    pub max_restarts: u32,
    /// Delay between attempts.
    pub retry_delay: Duration,
    /// Total time allowed to recover from the first failure.
    pub recovery_deadline: Duration,
    /// Grace allowed for the current attempt to stop after cancellation.
    pub shutdown_deadline: Duration,
}

/// Spawn a non-reconstructible owner and publish an unexpected exit as live
/// degradation. The future itself must observe `shutdown` and release its
/// resources; the wrapper never duplicates ownership by respawning it.
pub fn spawn_owned<F>(
    name: &'static str,
    critical: bool,
    evidence: ReadinessTracker,
    shutdown: CancellationToken,
    task: F,
) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        evidence.register(name, critical).await;
        evidence.ready(name).await;
        task.await;
        if shutdown.is_cancelled() {
            evidence.stopped(name, None).await;
        } else {
            evidence
                .degraded(name, "task exited unexpectedly before shutdown")
                .await;
        }
    })
}

/// Spawn a task whose closure reconstructs all owned state on every attempt.
/// Failures restart only within the explicit count and wall-clock budget.
pub fn spawn_reconstructible<Factory, Task>(
    name: &'static str,
    critical: bool,
    evidence: ReadinessTracker,
    shutdown: CancellationToken,
    budget: RestartBudget,
    factory: Factory,
) -> JoinHandle<()>
where
    Factory: Fn(CancellationToken) -> Task + Send + Sync + 'static,
    Task: Future<Output = Result<(), String>> + Send + 'static,
{
    tokio::spawn(async move {
        evidence.register(name, critical).await;
        let mut recovery_started: Option<Instant> = None;
        let mut restarts_in_window = 0u32;
        loop {
            evidence.ready(name).await;
            let attempt_started = Instant::now();
            let attempt_shutdown = shutdown.child_token();
            let task = factory(attempt_shutdown.clone());
            tokio::pin!(task);

            let failure = tokio::select! {
                result = &mut task => match result {
                    Ok(()) => "task exited unexpectedly before shutdown".to_string(),
                    Err(error) => error,
                },
                _ = shutdown.cancelled() => {
                    attempt_shutdown.cancel();
                    if tokio::time::timeout(budget.shutdown_deadline, &mut task).await.is_err() {
                        evidence.stopped(name, Some("shutdown deadline exceeded".to_string())).await;
                    } else {
                        evidence.stopped(name, None).await;
                    }
                    return;
                }
            };

            evidence.degraded(name, failure.clone()).await;
            // A task that stayed up for a full recovery window starts a new
            // incident; its historical restart_count remains visible, but an
            // ancient transient failure must not consume today's budget.
            if attempt_started.elapsed() >= budget.recovery_deadline {
                recovery_started = None;
                restarts_in_window = 0;
            }
            let recovery_started = *recovery_started.get_or_insert_with(Instant::now);
            if restarts_in_window >= budget.max_restarts
                || recovery_started.elapsed() >= budget.recovery_deadline
            {
                evidence
                    .stopped(name, Some(format!("restart budget exhausted: {failure}")))
                    .await;
                return;
            }

            tokio::select! {
                _ = shutdown.cancelled() => {
                    evidence.stopped(name, None).await;
                    return;
                }
                _ = tokio::time::sleep(budget.retry_delay) => {}
            }
            restarts_in_window = restarts_in_window.saturating_add(1);
            evidence.record_restart(name).await;
            evidence.register(name, critical).await;
        }
    })
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn critical_subsystem_state_controls_node_readiness() {
        let evidence = ReadinessTracker::new();
        evidence.register("agent", true).await;
        evidence.register("metrics", false).await;

        let starting = evidence.snapshot().await;
        assert!(!starting.ready);
        assert_eq!(starting.subsystems[0].state, SubsystemState::Starting);

        evidence.ready("agent").await;
        evidence.degraded("metrics", "collector unavailable").await;
        let ready = evidence.snapshot().await;
        assert!(
            ready.ready,
            "a non-critical degradation must stay visible without fencing"
        );

        evidence.degraded("agent", "command loop exited").await;
        let degraded = evidence.snapshot().await;
        assert!(!degraded.ready);
        assert_eq!(
            degraded.subsystems[0].last_error.as_deref(),
            Some("command loop exited")
        );
        assert!(degraded.subsystems[0].last_error_unix_ms.is_some());

        evidence.stopped("agent", None).await;
        assert_eq!(
            evidence.snapshot().await.subsystems[0].state,
            SubsystemState::Stopped
        );
    }

    #[tokio::test]
    async fn reconstructible_task_restarts_within_its_deadline() {
        let evidence = ReadinessTracker::new();
        let shutdown = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let run_attempts = Arc::clone(&attempts);
        let handle = spawn_reconstructible(
            "refresh",
            false,
            evidence.clone(),
            shutdown.clone(),
            RestartBudget {
                max_restarts: 2,
                retry_delay: Duration::from_millis(1),
                recovery_deadline: Duration::from_secs(1),
                shutdown_deadline: Duration::from_millis(50),
            },
            move |_attempt_shutdown| {
                let run_attempts = Arc::clone(&run_attempts);
                async move {
                    if run_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err("transient failure".to_string())
                    } else {
                        std::future::pending::<Result<(), String>>().await
                    }
                }
            },
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = evidence.snapshot().await;
                if attempts.load(Ordering::SeqCst) == 2
                    && snapshot.subsystems[0].state == SubsystemState::Ready
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(evidence.snapshot().await.subsystems[0].restart_count, 1);

        shutdown.cancel();
        handle.await.unwrap();
        assert_eq!(
            evidence.snapshot().await.subsystems[0].state,
            SubsystemState::Stopped
        );
    }

    #[tokio::test]
    async fn non_reconstructible_exit_is_degraded_without_restart() {
        let evidence = ReadinessTracker::new();
        let shutdown = CancellationToken::new();
        let handle = spawn_owned("socket owner", true, evidence.clone(), shutdown, async {});
        handle.await.unwrap();

        let snapshot = evidence.snapshot().await;
        assert!(!snapshot.ready);
        assert_eq!(snapshot.subsystems[0].state, SubsystemState::Degraded);
        assert_eq!(snapshot.subsystems[0].restart_count, 0);
    }
}
