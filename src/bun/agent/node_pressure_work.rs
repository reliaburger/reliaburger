//! Node pressure faults, started and cleared off the agent loop (#351,
//! stage 3).
//!
//! A node-pressure fault runs a helper copy of `bun` in its own cgroup. The
//! helper has up to four seconds to allocate its ballast and say it's ready,
//! and clearing it waits up to two for the helper to exit, then removes its
//! cgroup. All of that used to happen inside the turn that injected or
//! cleared the fault.
//!
//! The controller now sits behind a `tokio::sync::Mutex` shared with the
//! tasks that drive it. The loop checks a request against the controller
//! (`try_lock`, so it never waits) and registers the fault, a task starts or
//! stops the helper with the controller locked, and the result comes back as
//! a [`FollowUp`]. The caller is answered then, so an injected fault is
//! reported once its helper runs, and a cleared one once its helper is gone.
//! The mutex serialises a clear behind a start still in flight, so the two
//! can't race over the same helper.

use std::sync::Arc;

use tokio::sync::oneshot;

use super::follow_ups::FollowUp;
use super::{BunAgent, BunError, FaultClearance, Grill};
use crate::smoker::node_pressure::NodePressureController;
use crate::smoker::types::{FaultId, FaultReversal, FaultRule, FaultSummary};

/// The node-pressure controller, shared between the loop and its tasks.
pub(super) type SharedPressure = Arc<tokio::sync::Mutex<NodePressureController>>;

/// Which half of fencing a pressure fault failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PressureFenceStep {
    /// Stopping the helper; the fault stays registered.
    Clear,
    /// Proving no helper is left; the fault is gone, the slot isn't free.
    Confirm,
}

/// A node-pressure task's result, for the loop to finish.
pub(super) enum PressureDone {
    /// The helper for `rule_id` started, or didn't.
    Started {
        rule_id: FaultId,
        result: Result<(), String>,
        response: oneshot::Sender<Result<FaultSummary, BunError>>,
    },
    /// `ClearFault` stopped the helper for `rule`, or didn't.
    Cleared {
        rule: Box<FaultRule>,
        reservation: Option<u64>,
        result: Result<(), String>,
        response: oneshot::Sender<Result<FaultClearance, BunError>>,
    },
    /// The leader's fence of a pressure grant stopped its helper and
    /// confirmed none is left, or one of those failed.
    Fenced {
        grant: Box<crate::smoker::reservation::NodeFaultReservation>,
        fenced: Option<FaultId>,
        result: Result<(), (PressureFenceStep, String)>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Whether a pressure fault may start now. Never waits: a controller a
    /// task holds means another pressure operation is in flight, and
    /// pressure faults don't run side by side anyway.
    pub(super) fn check_node_pressure(
        &self,
        rule: &FaultRule,
        cpu_percentage: u8,
        memory_percentage: u8,
    ) -> Result<(), String> {
        if rule.duration_ns == 0 {
            return Err("node pressure requires a non-zero duration".to_string());
        }
        let controller = self.node_pressure.try_lock().map_err(|_| {
            "another node-pressure operation is in progress; try again shortly".to_string()
        })?;
        controller.check_apply(cpu_percentage, memory_percentage)
    }

    /// Start the helper for a registered pressure fault from a task, and
    /// answer `response` when it's running (or isn't).
    pub(super) fn spawn_node_pressure_start(
        &mut self,
        rule_id: FaultId,
        cpu_percentage: u8,
        memory_percentage: u8,
        response: oneshot::Sender<Result<FaultSummary, BunError>>,
    ) {
        let controller = Arc::clone(&self.node_pressure);
        self.follow_ups.spawn(async move {
            let result = controller
                .lock()
                .await
                .apply(rule_id, cpu_percentage, memory_percentage)
                .await;
            FollowUp::NodePressure(PressureDone::Started {
                rule_id,
                result,
                response,
            })
        });
    }

    /// Stop a pressure fault's helper from a task, when nobody waits for
    /// the answer: expiry, a bulk clear, shutdown. Failures are logged.
    pub(super) fn spawn_node_pressure_clear(&self, id: FaultId) {
        let controller = Arc::clone(&self.node_pressure);
        tokio::spawn(async move {
            if let Err(error) = controller.lock().await.clear(id).await {
                eprintln!("smoker: clear node pressure for {id} failed: {error}");
            }
        });
    }

    /// Answer `ClearFault` for a pressure fault once its helper is gone.
    pub(super) fn spawn_node_pressure_clearance(
        &mut self,
        rule: FaultRule,
        reservation: Option<u64>,
        response: oneshot::Sender<Result<FaultClearance, BunError>>,
    ) {
        let controller = Arc::clone(&self.node_pressure);
        self.follow_ups.spawn(async move {
            let result = controller.lock().await.clear(rule.id).await;
            FollowUp::NodePressure(PressureDone::Cleared {
                rule: Box::new(rule),
                reservation,
                result,
                response,
            })
        });
    }

    /// Finish fencing a pressure grant: stop the fenced fault's helper, if
    /// this node fenced one, and confirm no helper is left.
    pub(super) fn spawn_node_pressure_fence(
        &mut self,
        grant: crate::smoker::reservation::NodeFaultReservation,
        fenced: Option<FaultId>,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        let controller = Arc::clone(&self.node_pressure);
        self.follow_ups.spawn(async move {
            let result = async {
                let mut controller = controller.lock().await;
                if let Some(id) = fenced {
                    controller
                        .clear(id)
                        .await
                        .map_err(|error| (PressureFenceStep::Clear, error))?;
                }
                controller
                    .confirm_no_helpers()
                    .await
                    .map_err(|error| (PressureFenceStep::Confirm, error))
            }
            .await;
            FollowUp::NodePressure(PressureDone::Fenced {
                grant: Box::new(grant),
                fenced,
                result,
                response,
            })
        });
    }

    /// Retry removing a pressure cgroup that lingered after its helper
    /// died, from a task. Skipped while a task holds the controller.
    pub(super) fn retry_node_pressure_cleanup(&self) {
        let pending = self
            .node_pressure
            .try_lock()
            .is_ok_and(|controller| controller.has_pending_cleanup());
        if !pending {
            return;
        }
        let controller = Arc::clone(&self.node_pressure);
        tokio::spawn(async move {
            controller.lock().await.retry_pending_cleanup().await;
        });
    }

    /// Apply a node-pressure task's result and answer its caller.
    pub(super) async fn finish_node_pressure(&mut self, done: PressureDone) {
        match done {
            PressureDone::Started {
                rule_id,
                result: Ok(()),
                response,
            } => match self.fault_registry.get(rule_id).cloned() {
                Some(rule) => {
                    self.record_reversal(rule_id, FaultReversal::NodePressure);
                    let _ = response.send(Ok(FaultSummary::from(&rule)));
                }
                // Cleared while its helper started: stop that helper too.
                None => {
                    self.spawn_node_pressure_clear(rule_id);
                    let _ = response.send(Err(BunError::FaultRejected {
                        reason: "the fault was cleared while its helper started".into(),
                    }));
                }
            },
            PressureDone::Started {
                rule_id,
                result: Err(reason),
                response,
            } => {
                self.fault_registry.remove(rule_id);
                self.reconcile_network_faults().await;
                let _ = response.send(Err(BunError::FaultRejected { reason }));
            }
            PressureDone::Cleared {
                result: Err(reason),
                response,
                ..
            } => {
                let _ = response.send(Err(BunError::FaultRejected { reason }));
            }
            PressureDone::Cleared {
                rule,
                reservation,
                result: Ok(()),
                response,
            } => {
                self.fault_registry.remove(rule.id);
                self.reconcile_network_faults().await;
                self.publish_dns_faults();
                let _ = response.send(Ok(FaultClearance {
                    message: format!("cleared fault {} ({})", rule.id, rule.fault_type),
                    reservation,
                }));
            }
            PressureDone::Fenced {
                grant,
                fenced,
                result,
                response,
            } => {
                // Once the helper is stopped the fault is gone, whether or
                // not the confirmation that followed succeeded.
                if !matches!(result, Err((PressureFenceStep::Clear, _)))
                    && let Some(id) = fenced
                {
                    self.fault_registry.remove(id);
                }
                let answer = match result {
                    Ok(()) => {
                        self.release_node_fault_slot(&grant);
                        Ok(())
                    }
                    Err((_, reason)) => Err(BunError::FaultRejected { reason }),
                };
                let _ = response.send(answer);
            }
        }
    }
}
