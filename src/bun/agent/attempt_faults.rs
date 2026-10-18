//! Faults against job and task-array attempts (#704).
//!
//! An app's instances live in the supervisor; a job's attempts live in the
//! task runtime, which lists each one in [`LiveAttempts`] while it runs.
//! This module decides whether a workload fault targets an app or a job,
//! from what this node runs, and refuses what can't be confined to one task.

use super::*;
use crate::bun::live_attempts::LiveAttempt;
use crate::smoker::types::{FaultRequest, FaultRule, FaultType, WorkloadKind};

/// Why a fault can't reach one task of a job in a reusable executor.
fn shared_executor_refusal(fault: &FaultType, attempt: &LiveAttempt, executor: &str) -> String {
    format!(
        "{fault} can't be confined to task {} of {}: it runs in reusable executor {executor}, \
         whose cgroup and network the job's other tasks share; only kill and pause act on one \
         task there",
        attempt.index, attempt.job
    )
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Settle whether `request` targets an app or a job on this node, and
    /// refuse a fault that would reach more than the task it names.
    ///
    /// The supervisor wins: if this node runs an app by that name, the
    /// target is that app, whatever the request says. Otherwise this node's
    /// attempts decide. With neither, a request the API already scoped to a
    /// job keeps that scope. So a caller can't label an app as a job to step
    /// around the replica rail.
    pub(super) fn scope_local_workload_fault(
        &self,
        request: &mut FaultRequest,
    ) -> Result<(), String> {
        if request.fault_type.is_node_targeted() {
            return Ok(());
        }
        let Some(namespace) = request.namespace.clone() else {
            return Ok(());
        };
        let runs_app = self.supervisor.list_instances().iter().any(|instance| {
            instance.app_name == request.target_service && instance.namespace == namespace
        });
        let attempts = self
            .live_attempts
            .matching(&namespace, &request.target_service, None);
        if runs_app {
            request.workload = WorkloadKind::App;
            if request.task_index.is_some() {
                return Err(format!(
                    "{} is an app; a task index only scopes a job or task array",
                    request.target_service
                ));
            }
        } else if let Some(attempt) = attempts.first() {
            request.workload = attempt.kind();
        }
        if request.workload.runs_to_completion() {
            crate::smoker::routing::scope_to_task(request).map_err(|error| error.to_string())?;
        }
        self.refuse_shared_executor_scope(request, &namespace)
    }

    /// Refuse a resource fault on, or a network fault from, a command in a
    /// reusable executor. Its cgroup and network belong to the executor, so
    /// the fault would reach the tasks that run there after it as well.
    fn refuse_shared_executor_scope(
        &self,
        request: &FaultRequest,
        namespace: &str,
    ) -> Result<(), String> {
        let scoped = |job: &str, task_index| {
            self.live_attempts
                .matching(namespace, job, task_index)
                .into_iter()
                .find_map(|attempt| {
                    let executor = attempt.shared_executor.clone()?;
                    Some(shared_executor_refusal(
                        &request.fault_type,
                        &attempt,
                        &executor.0,
                    ))
                })
        };
        if request.workload.runs_to_completion()
            && !matches!(
                request.fault_type,
                FaultType::Kill { .. } | FaultType::Pause | FaultType::Resume
            )
            && let Some(reason) = scoped(&request.target_service, request.task_index)
        {
            return Err(reason);
        }
        if let Some(source) = request.fault_type.source_app()
            && let Some(reason) = scoped(source, None)
        {
            return Err(reason);
        }
        Ok(())
    }

    /// The attempts a job fault reaches on this node.
    pub(super) fn attempt_targets(&self, rule: &FaultRule) -> Vec<LiveAttempt> {
        if !rule.workload.runs_to_completion() {
            return Vec::new();
        }
        let Some(namespace) = rule.namespace.as_deref() else {
            return Vec::new();
        };
        self.live_attempts
            .matching(namespace, &rule.target_service, rule.task_index)
    }
}
