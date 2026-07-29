//! The chaos scenarios.
//!
//! The suite is deliberately stricter than ordinary capability-driven tests:
//! node failure and pressure are prerequisites, not optional green skips.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bun::capabilities::{Capability, CapabilityState, ClusterCapabilities};
use crate::relish::client::BunClient;
use crate::smoker::types::{FaultRequest, FaultSummary};
use crate::testkit::deadline::Deadline;
use crate::testkit::report::CleanupOutcome;
use crate::testkit::safety::OperationPermission;

mod scenarios;

use super::registry::TestCase;

/// Operator consent flags which can only narrow server policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChaosFlags {
    /// Skip the interactive prompt. This acknowledges effects but grants no
    /// role or operation permission.
    pub yes: bool,
}

/// Why the chaos suite refused to touch the cluster.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefusalReason {
    #[error("chaos suite requires at least 3 nodes (found {found})")]
    TooFewNodes { found: u32 },
    #[error("chaos suite requires fresh available capability {capability} ({state})")]
    MissingCapability {
        capability: Capability,
        state: &'static str,
    },
    #[error("chaos suite requires server policy operation {operation}")]
    MissingOperation { operation: OperationPermission },
    #[error(
        "cluster is protected; server policy must set allow_protected_mutation = true (there is no client override)"
    )]
    ProtectedCluster,
    #[error("non-interactive chaos requires --yes")]
    NonInteractiveConfirmation,
    #[error("interactive chaos requires confirmation")]
    InteractiveConfirmation,
    #[error("chaos suite needs the digest-pinned runc/Apple container workload")]
    NoHermeticWorkload,
}

/// Check every suite-wide prerequisite before creating a lease or fault.
///
/// A missing destructive primitive is a refusal rather than a skipped
/// scenario. Otherwise `relish test --chaos` could print green while never
/// failing a node, which rather defeats the exercise.
pub fn chaos_preflight(
    capabilities: &ClusterCapabilities,
    flags: ChaosFlags,
    is_tty: bool,
) -> Result<(), RefusalReason> {
    if capabilities.node_count < 3 {
        return Err(RefusalReason::TooFewNodes {
            found: capabilities.node_count,
        });
    }

    for operation in [
        OperationPermission::ProvisionIsolatedWorkloads,
        OperationPermission::AlterNodeState,
        OperationPermission::SaturateCapacity,
    ] {
        if !capabilities
            .test_policy
            .allowed_operations
            .contains(&operation)
        {
            return Err(RefusalReason::MissingOperation { operation });
        }
    }
    if capabilities.test_policy.safety_class.is_protected()
        && !capabilities.test_policy.allow_protected_mutation
    {
        return Err(RefusalReason::ProtectedCluster);
    }

    for capability in [
        Capability::MultiNode,
        Capability::NodeKill,
        Capability::NodePressure,
    ] {
        let state = capabilities.state(capability);
        if state != CapabilityState::Available {
            let state = match state {
                CapabilityState::Available => unreachable!(),
                CapabilityState::Unavailable => "unavailable",
                CapabilityState::Unknown => "unknown",
            };
            return Err(RefusalReason::MissingCapability { capability, state });
        }
    }
    if capabilities.state(Capability::ContainerRuntime) != CapabilityState::Available {
        return Err(RefusalReason::NoHermeticWorkload);
    }
    if !flags.yes {
        return Err(if is_tty {
            RefusalReason::InteractiveConfirmation
        } else {
            RefusalReason::NonInteractiveConfirmation
        });
    }
    Ok(())
}

#[derive(Clone)]
struct OwnedFault {
    id: u64,
    owner: BunClient,
    owner_node: Option<String>,
}

/// Exact fault ownership shared by a scenario task and the runner.
///
/// The runner retains this handle when it gives a clone to the case body, so
/// timeout and panic cannot skip cleanup. Each entry carries the direct client
/// for the node-local fault id; cleanup never issues a blanket clear.
#[derive(Clone, Default)]
pub struct ChaosGuard {
    faults: Arc<Mutex<Vec<OwnedFault>>>,
}

impl ChaosGuard {
    /// Inject through an owning node and retain the returned fault id.
    pub async fn inject_fault(
        &self,
        owner: BunClient,
        request: &FaultRequest,
    ) -> Result<FaultSummary, String> {
        let summary = owner
            .inject_fault(request)
            .await
            .map_err(|error| format!("fault injection failed: {error}"))?;
        let owner_node = summary.target_node.clone();
        self.track(owner, &summary, owner_node).await;
        Ok(summary)
    }

    /// Inject a council partition and retain its additive legacy-API fault id.
    pub async fn inject_partition(
        &self,
        owner: BunClient,
        owner_node: &str,
        peers: &[String],
        duration_seconds: u64,
    ) -> Result<FaultSummary, String> {
        let summary = owner
            .inject_partition(peers, duration_seconds, true)
            .await
            .map_err(|error| format!("council partition failed: {error}"))?;
        self.track(owner, &summary, Some(owner_node.to_string()))
            .await;
        Ok(summary)
    }

    async fn track(&self, owner: BunClient, summary: &FaultSummary, owner_node: Option<String>) {
        self.faults.lock().await.push(OwnedFault {
            id: summary.id,
            owner,
            owner_node,
        });
    }

    /// Reverse every fault this guard owns, newest first.
    pub async fn cleanup(&self, deadline: Deadline) -> CleanupOutcome {
        let mut owned = {
            let mut faults = self.faults.lock().await;
            std::mem::take(&mut *faults)
        };
        if owned.is_empty() {
            return CleanupOutcome::NotRequired;
        }
        owned.reverse();

        let mut retry = Vec::new();
        let mut failed = Vec::new();
        let mut unknown = Vec::new();
        for fault in owned {
            match deadline
                .run(
                    "fault cleanup",
                    fault
                        .owner
                        .clear_fault(fault.id, fault.owner_node.as_deref(), false),
                )
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(
                    crate::relish::RelishError::AgentUnreachable
                    | crate::relish::RelishError::RequestTimeout,
                ))
                | Err(_) => {
                    unknown.push(fault.id.to_string());
                    retry.push(fault);
                }
                Ok(Err(error)) => {
                    failed.push(format!("{}: {error}", fault.id));
                    retry.push(fault);
                }
            }
        }
        if !retry.is_empty() {
            self.faults.lock().await.extend(retry);
        }
        if !failed.is_empty() {
            CleanupOutcome::Failed {
                reason: format!("owned fault cleanup failed: {}", failed.join("; ")),
            }
        } else if !unknown.is_empty() {
            CleanupOutcome::Unknown {
                reason: format!(
                    "could not confirm cleanup for owned fault id(s) {}",
                    unknown.join(", ")
                ),
            }
        } else {
            CleanupOutcome::Confirmed
        }
    }
}

/// The chaos scenarios, selected by `--chaos`.
pub fn scenarios() -> Vec<TestCase> {
    scenarios::all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::capabilities::{
        Capability, CapabilityEvidence, CapabilityState, ClusterCapabilities,
    };
    use crate::testkit::safety::{ClusterSafetyClass, ClusterTestPolicy, OperationPermission};
    use axum::extract::{Path, State};
    use axum::routing::delete;
    use axum::{Json, Router};

    fn capabilities(nodes: u32) -> ClusterCapabilities {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut report = ClusterCapabilities {
            node_count: nodes,
            cluster_name: "test-cluster".to_string(),
            expires_at_unix_ms: now + 60_000,
            test_policy: ClusterTestPolicy {
                safety_class: ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    OperationPermission::ProvisionIsolatedWorkloads,
                    OperationPermission::AlterNodeState,
                    OperationPermission::SaturateCapacity,
                ]),
                max_node_pressure_cpu_percent: 80,
                max_node_pressure_memory_percent: 90,
                ..ClusterTestPolicy::default()
            },
            ..ClusterCapabilities::default()
        };
        report.capabilities = [
            Capability::MultiNode,
            Capability::NodeKill,
            Capability::NodePressure,
            Capability::ContainerRuntime,
        ]
        .into_iter()
        .map(|capability| CapabilityEvidence {
            capability,
            state: CapabilityState::Available,
            observed_at_unix_ms: now,
            expires_at_unix_ms: now + 60_000,
            details: Vec::new(),
        })
        .collect();
        report
    }

    #[test]
    fn refuses_fewer_than_three_nodes() {
        let error = chaos_preflight(&capabilities(1), ChaosFlags { yes: true }, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "chaos suite requires at least 3 nodes (found 1)"
        );
    }

    #[test]
    fn requires_yes_when_not_a_tty() {
        let error =
            chaos_preflight(&capabilities(3), ChaosFlags { yes: false }, false).unwrap_err();
        assert!(error.to_string().contains("--yes"), "{error}");
    }

    #[test]
    fn protected_clusters_need_the_server_gate_not_a_client_override() {
        let mut caps = capabilities(3);
        caps.test_policy.safety_class = ClusterSafetyClass::Production;
        let error = chaos_preflight(&caps, ChaosFlags { yes: true }, false).unwrap_err();
        assert!(
            error.to_string().contains("server policy"),
            "unexpected refusal: {error}"
        );

        caps.test_policy.allow_protected_mutation = true;
        assert!(chaos_preflight(&caps, ChaosFlags { yes: true }, false).is_ok());
    }

    #[test]
    fn missing_required_operation_is_a_refusal_not_a_green_skip() {
        let mut caps = capabilities(3);
        caps.test_policy
            .allowed_operations
            .remove(&OperationPermission::SaturateCapacity);
        let error = chaos_preflight(&caps, ChaosFlags { yes: true }, false).unwrap_err();
        assert!(error.to_string().contains("saturate_capacity"), "{error}");
    }

    #[test]
    fn unavailable_node_pressure_is_a_refusal_not_a_green_skip() {
        let mut caps = capabilities(3);
        caps.capabilities
            .iter_mut()
            .find(|evidence| evidence.capability == Capability::NodePressure)
            .unwrap()
            .state = CapabilityState::Unavailable;
        let error = chaos_preflight(&caps, ChaosFlags { yes: true }, false).unwrap_err();
        assert!(error.to_string().contains("node_pressure"), "{error}");
    }

    #[test]
    fn catalogue_contains_the_five_named_scenarios() {
        let names: Vec<_> = scenarios().into_iter().map(|case| case.name).collect();
        assert_eq!(
            names,
            vec![
                "leader_failure_elects_new_leader_and_cluster_recovers",
                "dead_worker_node_has_workloads_rescheduled",
                "minority_partition_degrades_and_heals",
                "resource_exhaustion_degrades_gracefully",
                "node_death_during_deploy_ends_clean",
            ]
        );
    }

    #[tokio::test]
    async fn cleanup_deletes_only_the_fault_ids_the_guard_owns() {
        async fn clear(
            Path(id): Path<u64>,
            State(cleared): State<Arc<Mutex<Vec<u64>>>>,
        ) -> Json<serde_json::Value> {
            cleared.lock().await.push(id);
            Json(serde_json::json!({ "message": "cleared" }))
        }

        let cleared = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/fault/{id}", delete(clear))
            .with_state(Arc::clone(&cleared));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let owner = BunClient::new(&format!("http://{address}"));
        let guard = ChaosGuard::default();
        for id in [7, 9] {
            guard
                .track(
                    owner.clone(),
                    &FaultSummary {
                        id,
                        fault_type: "node-kill".to_string(),
                        target_service: String::new(),
                        target_instance: None,
                        target_node: Some("node-a".to_string()),
                        remaining_secs: 30,
                        injected_by: "test".to_string(),
                    },
                    Some("node-a".to_string()),
                )
                .await;
        }

        assert_eq!(
            guard
                .cleanup(Deadline::after(std::time::Duration::from_secs(2)).unwrap())
                .await,
            CleanupOutcome::Confirmed
        );
        assert_eq!(*cleared.lock().await, vec![9, 7]);
    }
}
