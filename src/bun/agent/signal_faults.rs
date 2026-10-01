//! Kill, pause and resume faults, signalled off the agent loop (#351,
//! stage 3).
//!
//! These faults signal their targets' processes, so first they ask the
//! runtime for each target's pid. Under a busy runtime that's hundreds of
//! milliseconds a target. The loop picks the targets (a walk over its own
//! map) and a task reads the pids and sends the signals; the result comes
//! back as a [`FollowUp`], and only then is the caller told the fault is in.
//! A pause records which processes it froze there, so clearing or expiring
//! it thaws exactly those.

use std::time::Duration;

use tokio::sync::oneshot;

use super::follow_ups::FollowUp;
use super::{BunAgent, BunError, Grill, InstanceId};
use crate::smoker::types::{FaultId, FaultReversal, FaultRule, FaultSummary, FaultType};

/// How long a task waits for the runtime to name the targets' pids. A
/// target it can't name by then is left alone, as one with no pid always
/// was.
const PID_READ_PATIENCE: Duration = Duration::from_secs(10);

/// Which signal a fault sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    /// SIGKILL up to `count` targets (0: all).
    Kill { count: u32 },
    /// SIGSTOP every target, remembering which froze.
    Pause,
    /// SIGCONT every target.
    Resume,
}

impl Signal {
    /// The signal a fault sends, if it is one of these.
    pub(super) fn of(fault: &FaultType) -> Option<Self> {
        match fault {
            FaultType::Kill { count } => Some(Signal::Kill { count: *count }),
            FaultType::Pause => Some(Signal::Pause),
            FaultType::Resume => Some(Signal::Resume),
            _ => None,
        }
    }

    pub(super) fn count(self) -> u32 {
        match self {
            Signal::Kill { count } => count,
            Signal::Pause | Signal::Resume => 0,
        }
    }
}

/// A signal fault's task result, for the loop to finish.
pub(super) struct Signalled {
    rule_id: FaultId,
    /// The pids a pause froze; `None` for a kill or a resume.
    result: Result<Option<Vec<i32>>, String>,
    response: oneshot::Sender<Result<FaultSummary, BunError>>,
}

/// Read each target's pid at once until `deadline`, keeping at most
/// `count` (0: all). Reads that don't finish in time are left out.
pub(super) async fn read_pids<G: Grill>(
    grill: &G,
    ids: &[InstanceId],
    count: u32,
    deadline: tokio::time::Instant,
) -> Vec<u32> {
    let reads = ids.iter().map(|id| async move {
        tokio::time::timeout_at(deadline, grill.pid(id))
            .await
            .ok()
            .flatten()
    });
    // `timeout_at` polls the reads before its clock, so at the deadline the
    // reads that finished still count.
    let mut pids: Vec<u32> =
        tokio::time::timeout_at(deadline, futures_util::future::join_all(reads))
            .await
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect();
    if count > 0 {
        pids.truncate(count as usize);
    }
    pids
}

/// Send `signal` to `pids`. A kill or a pause with nobody to signal fails;
/// a resume with nobody to thaw doesn't.
pub(super) fn send(
    signal: Signal,
    pids: &[u32],
    service: &str,
) -> Result<Option<Vec<i32>>, String> {
    if pids.is_empty() && signal != Signal::Resume {
        return Err(format!("no running instances of {service}"));
    }
    let mut paused = Vec::new();
    for &pid in pids {
        let pid = pid as i32;
        let (sent, verb) = match signal {
            Signal::Kill { .. } => (crate::smoker::process::kill_process(pid), "kill"),
            Signal::Pause => (crate::smoker::process::pause_process(pid), "pause"),
            Signal::Resume => (crate::smoker::process::resume_process(pid), "resume"),
        };
        match sent {
            Ok(()) => paused.push(pid),
            Err(error) => eprintln!("smoker: {verb} {pid} failed: {error}"),
        }
    }
    Ok((signal == Signal::Pause).then_some(paused))
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Running instances a fault targets: its service (or one instance of
    /// it) in its namespace, past their image pull.
    pub(super) fn fault_targets(&self, rule: &FaultRule) -> Vec<InstanceId> {
        self.supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                instance.app_name == rule.target_service
                    && rule.matches_namespace(&instance.namespace)
                    && rule
                        .target_instance
                        .as_ref()
                        .is_none_or(|target| &instance.id.0 == target)
                    && !instance.is_being_created()
            })
            .map(|instance| instance.id.clone())
            .collect()
    }

    /// Read the targets' pids and signal them from a task, and answer
    /// `response` once the signals are sent.
    pub(super) fn spawn_signal_fault(
        &mut self,
        rule: &FaultRule,
        signal: Signal,
        response: oneshot::Sender<Result<FaultSummary, BunError>>,
    ) {
        let ids = self.fault_targets(rule);
        let grill = self.supervisor.grill().clone();
        let rule_id = rule.id;
        let service = rule.target_service.clone();
        self.follow_ups.spawn(async move {
            let deadline = tokio::time::Instant::now() + PID_READ_PATIENCE;
            let pids = read_pids(&grill, &ids, signal.count(), deadline).await;
            FollowUp::Signalled(Signalled {
                rule_id,
                result: send(signal, &pids, &service),
                response,
            })
        });
    }

    /// Record what a signal fault did and answer its caller.
    pub(super) fn finish_signal_fault(&mut self, done: Signalled) {
        let Signalled {
            rule_id,
            result,
            response,
        } = done;
        let answer = match (result, self.fault_registry.get(rule_id).cloned()) {
            (Err(reason), _) => {
                self.fault_registry.remove(rule_id);
                Err(BunError::FaultRejected { reason })
            }
            (Ok(paused), Some(rule)) => {
                if let Some(paused) = paused {
                    self.record_reversal(rule_id, FaultReversal::Pause(paused));
                }
                Ok(FaultSummary::from(&rule))
            }
            // Cleared while the signals were on their way: thaw what this
            // pause froze, since no reversal will.
            (Ok(paused), None) => {
                for pid in paused.unwrap_or_default() {
                    if let Err(error) = crate::smoker::process::resume_process(pid) {
                        eprintln!("smoker: resume {pid} failed: {error}");
                    }
                }
                Err(BunError::FaultRejected {
                    reason: "the fault was cleared while it was being applied".into(),
                })
            }
        };
        let _ = response.send(answer);
    }
}
