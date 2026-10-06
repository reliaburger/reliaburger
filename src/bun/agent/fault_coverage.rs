//! Answering a fault injection only once the fault reaches every caller
//! (#625).
//!
//! A partition from one app is keyed in `fault_connect_map` by each of that
//! app's instances' cgroup ids, and the runtime names a cgroup only when
//! asked. The turn that injects the fault asks under its 500 ms budget, and a
//! caller it can't name in time gets no key and no connection cut: new dials
//! from it still go through, and so does the pool it already holds open. The
//! #450 fix made the injection wait for its cuts, but a caller with no key has
//! no cut to wait for. So the answer waits for the callers too: a task reads
//! their cgroups off the loop (the pattern [`super::signal_faults`] uses for
//! pids), the loop keys and cuts them when the task reports back, and only
//! then does the caller hear the fault is in.

use std::time::Duration;

use tokio::sync::oneshot;

use super::follow_ups::FollowUp;
use super::{BunAgent, BunError, Grill, InstanceId};
use crate::smoker::network::{LateCuts, applies_to_caller, keyed_by_caller_cgroup};
use crate::smoker::types::{FaultId, FaultSummary};

/// How long a task waits for the runtime to name the cgroups the injecting
/// turn couldn't. A caller still unnamed by then fails the injection.
const CALLER_CGROUP_PATIENCE: Duration = Duration::from_secs(10);

/// How many times an injection sends a task for unnamed callers before it
/// gives up. A caller that restarts while its cgroup is read needs another
/// read; one that keeps restarting fails the injection.
const MAX_CALLER_READS: u32 = 3;

/// A caller whose cgroup the runtime didn't name before its turn's deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingCaller {
    pub(super) id: InstanceId,
    pub(super) app: String,
    pub(super) namespace: String,
    /// The restart count the read was for; a restarted caller is read again.
    pub(super) restarts: u32,
}

/// An injected fault whose caller hasn't been answered yet.
pub(super) struct Injection {
    rule_id: FaultId,
    summary: FaultSummary,
    /// Connection cuts that must finish before the answer.
    late_cuts: LateCuts,
    /// Callers a task has already read, at the restart count it read them.
    /// They count as reached even if a later turn's own read runs out of
    /// time again: their key is in the map, or the runtime proved they have
    /// no cgroup to key.
    settled: Vec<(InstanceId, u32)>,
    /// How many tasks this injection has sent to read callers.
    reads: u32,
    response: oneshot::Sender<Result<FaultSummary, BunError>>,
}

impl Injection {
    /// An injection of `rule_id` that has left `late_cuts` running.
    pub(super) fn new(
        rule_id: FaultId,
        summary: FaultSummary,
        late_cuts: LateCuts,
        response: oneshot::Sender<Result<FaultSummary, BunError>>,
    ) -> Self {
        Self {
            rule_id,
            summary,
            late_cuts,
            settled: Vec::new(),
            reads: 0,
            response,
        }
    }
}

/// One caller's cgroup read: `None` when it ran out of patience, otherwise
/// the runtime's answer (an error rendered as text, since it crosses tasks).
type CgroupRead = Option<Result<Option<u64>, String>>;

/// What a task read for an injection's unnamed callers.
pub(super) struct CallersRead {
    injection: Injection,
    /// Each caller and what its read found.
    cgroups: Vec<(PendingCaller, CgroupRead)>,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Answer an injection once its cuts have run and it reaches every caller
    /// it acts on, sending a task for the callers this turn couldn't name.
    pub(super) async fn answer_fault_injection(&mut self, mut injection: Injection) {
        let Some(rule) = self.fault_registry.get(injection.rule_id) else {
            self.spawn_cuts(std::mem::take(&mut injection.late_cuts));
            let _ = injection.response.send(Err(BunError::FaultRejected {
                reason: "the fault was cleared while it was being applied".into(),
            }));
            return;
        };
        let unreached: Vec<PendingCaller> = if keyed_by_caller_cgroup(rule) {
            self.network_faults
                .pending_callers
                .iter()
                .filter(|caller| {
                    applies_to_caller(rule, &caller.app, &caller.namespace)
                        && !injection
                            .settled
                            .iter()
                            .any(|(id, restarts)| *id == caller.id && *restarts == caller.restarts)
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        if unreached.is_empty() {
            let Injection {
                summary,
                late_cuts,
                response,
                ..
            } = injection;
            if late_cuts.is_empty() {
                let _ = response.send(Ok(summary));
            } else {
                tokio::spawn(async move {
                    super::faults::finish_late_cuts(late_cuts).await;
                    let _ = response.send(Ok(summary));
                });
            }
            return;
        }
        if injection.reads >= MAX_CALLER_READS {
            let names: Vec<&str> = unreached
                .iter()
                .map(|caller| caller.id.0.as_str())
                .collect();
            let reason = format!(
                "the runtime didn't name the cgroup of {} in time, so the fault can't reach it",
                names.join(", ")
            );
            self.refuse_injection(injection, reason).await;
            return;
        }

        injection.reads += 1;
        let late_cuts = std::mem::take(&mut injection.late_cuts);
        let grill = self.supervisor.grill().clone();
        self.follow_ups.spawn(async move {
            let deadline = tokio::time::Instant::now() + CALLER_CGROUP_PATIENCE;
            let reads = futures_util::future::join_all(unreached.iter().map(|caller| {
                let grill = &grill;
                async move {
                    tokio::time::timeout_at(deadline, grill.workload_cgroup(&caller.id))
                        .await
                        .ok()
                        .map(|read| read.map_err(|error| error.to_string()))
                }
            }));
            // The cuts the injecting turn left run alongside the reads.
            let ((), reads) = tokio::join!(super::faults::finish_late_cuts(late_cuts), reads);
            FollowUp::CallersRead(CallersRead {
                injection,
                cgroups: unreached.into_iter().zip(reads).collect(),
            })
        });
    }

    /// Key and cut the callers a task read, then answer the injection if it
    /// now reaches every caller.
    pub(super) async fn finish_callers_read(&mut self, read: CallersRead) {
        let CallersRead {
            mut injection,
            cgroups,
        } = read;
        if self.fault_registry.get(injection.rule_id).is_none() {
            self.answer_fault_injection(injection).await;
            return;
        }
        let mut unread = Vec::new();
        let mut proven = Vec::new();
        for (caller, outcome) in cgroups {
            match outcome {
                None => unread.push(caller.id.0),
                Some(Ok(cgroup)) => {
                    if let Some(cgroup) = cgroup {
                        self.network_faults
                            .caller_cgroups
                            .insert(caller.id.clone(), (caller.restarts, cgroup));
                        proven.push(caller.id.0.clone());
                    }
                    injection.settled.push((caller.id, caller.restarts));
                }
                // As in `local_callers`: a runtime that can't prove a cgroup
                // leaves the caller out, and says why.
                Some(Err(error)) => {
                    eprintln!(
                        "smoker: caller {} has no provable cgroup: {error}",
                        caller.id
                    );
                    injection.settled.push((caller.id, caller.restarts));
                }
            }
        }
        if !unread.is_empty() {
            let reason = format!(
                "the runtime didn't name the cgroup of {} in {}s, so the fault can't reach it",
                unread.join(", "),
                CALLER_CGROUP_PATIENCE.as_secs()
            );
            self.refuse_injection(injection, reason).await;
            return;
        }

        // Key the callers the task named. This reconcile's own cuts that miss
        // the turn come back for this answer to wait on.
        if let Err(error) = self.reconcile_connect_faults().await {
            eprintln!("smoker: network fault reconcile: {error}");
        }
        injection
            .late_cuts
            .extend(std::mem::take(&mut self.network_faults.late_cuts));
        self.recut_named_callers(&mut injection, proven);
        self.answer_fault_injection(injection).await;
    }

    /// Cut the named callers' connections again, for this answer to wait on.
    ///
    /// A health tick may have keyed them while the task read their cgroups,
    /// and cut them from a task nobody waits for; this reconcile then found
    /// nothing to change and cut nothing. Destroying sockets that are already
    /// gone is a no-op, so cutting twice is harmless.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    fn recut_named_callers(&self, injection: &mut Injection, named: Vec<String>) {
        let Some(rule) = self.fault_registry.get(injection.rule_id) else {
            return;
        };
        let backends = super::faults::fault_backend_addresses(&self.merged_service_map(), rule);
        if backends.is_empty() {
            return;
        }
        let cuts: Vec<_> = named
            .into_iter()
            .map(|instance_id| crate::smoker::network::ConnectionCut {
                instance_id,
                backends: backends.clone(),
            })
            .collect();
        injection.late_cuts.extend(LateCuts::from(cuts));
    }

    /// Without the eBPF data path no fault keys a caller, so there's nothing
    /// to cut.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    fn recut_named_callers(&self, _injection: &mut Injection, _named: Vec<String>) {}

    /// Take an injection back and tell its caller why.
    async fn refuse_injection(&mut self, mut injection: Injection, reason: String) {
        self.fault_registry.remove(injection.rule_id);
        // Converging without the rule takes back every key it wrote.
        self.reconcile_network_faults().await;
        self.spawn_cuts(std::mem::take(&mut injection.late_cuts));
        let _ = injection
            .response
            .send(Err(BunError::FaultRejected { reason }));
    }

    /// Finish cuts nobody waits for from a task.
    fn spawn_cuts(&self, cuts: LateCuts) {
        if !cuts.is_empty() {
            tokio::spawn(super::faults::finish_late_cuts(cuts));
        }
    }
}
