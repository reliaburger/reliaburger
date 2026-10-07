//! Positive local execution evidence for an ordinary cluster apply reservation.
use super::*;
use sha2::{Digest, Sha256};

/// Exact durable generation and accepted specification captured at job startup.
#[derive(Debug, Clone)]
pub struct ClusterJobExecution {
    /// Original logical workload name.
    pub name: String,
    /// Namespace participating in the runtime identity.
    pub namespace: String,
    /// Durable generation that settlement must still observe.
    pub generation: u64,
    /// Digest of the accepted namespace, name and complete effective job spec.
    pub spec_digest: String,
}
/// One startup observation; terminal settlement still requires current evidence.
#[derive(Debug, Clone)]
pub struct ClusterJobReceipt {
    /// Captured executions keyed by their canonical physical runtime identity.
    pub executions: BTreeMap<String, ClusterJobExecution>,
}
/// Whether the original executions can release their replicated ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterJobSettlement {
    /// At least one exact generation remains active or has a retry pending.
    Pending,
    /// Every captured generation has a confirmed terminal exit or absence.
    Terminal,
    /// Uncertainty or changed ownership retains the original fence.
    Unknown(String),
}

fn spec_digest(namespace: &str, name: &str, spec: &JobSpec) -> Result<String, String> {
    let mut canonical = spec.clone();
    canonical.namespace = Some(namespace.into());
    let bytes =
        serde_json::to_vec(&(namespace, name, canonical)).map_err(|error| error.to_string())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    pub(super) fn capture_cluster_jobs(
        &self,
        config: &Config,
    ) -> Result<Arc<ClusterJobReceipt>, String> {
        if self.job_store_uncertain || self.scheduled_jobs_store_uncertain {
            return Err("job publication is uncertain".into());
        }
        let mut executions = BTreeMap::new();
        for (name, spec) in &config.job {
            if !spec.run_before.is_empty() {
                return Err("prerequisites cannot enter an ordinary job receipt".into());
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            let expected = spec_digest(namespace, name, spec)?;
            if spec.schedule.is_some() {
                return Err("cluster job receipts do not support recurring schedules".into());
            }
            let id = crate::grill::InstanceIdentity::new(namespace, name, 0).instance_id();
            let job = self
                .recorded_jobs
                .get(&id.0)
                .ok_or("job launch has no durable attempt receipt")?;
            if job.name != *name
                || job.namespace != namespace
                || job.generation == 0
                || spec_digest(namespace, name, &job.spec)? != expected
            {
                return Err("job attempt does not match the accepted apply".into());
            }
            executions.insert(
                id.0,
                ClusterJobExecution {
                    name: name.clone(),
                    namespace: namespace.into(),
                    generation: job.generation,
                    spec_digest: expected,
                },
            );
        }
        Ok(Arc::new(ClusterJobReceipt { executions }))
    }

    pub(super) fn cluster_jobs_settlement(
        &self,
        receipt: &ClusterJobReceipt,
    ) -> ClusterJobSettlement {
        use crate::bun::jobs::{JobPhase, MAX_RETRIES};
        if self.job_store_uncertain
            || self.scheduled_jobs_store_uncertain
            || receipt.executions.is_empty()
        {
            return ClusterJobSettlement::Unknown("job outcome publication is uncertain".into());
        }
        let mut pending = false;
        for (id, captured) in &receipt.executions {
            let Some(job) = self.recorded_jobs.get(id) else {
                return ClusterJobSettlement::Unknown(
                    "the original job receipt is no longer available".into(),
                );
            };
            if job.generation != captured.generation
                || job.name != captured.name
                || job.namespace != captured.namespace
                || spec_digest(&captured.namespace, &captured.name, &job.spec).as_deref()
                    != Ok(captured.spec_digest.as_str())
            {
                return ClusterJobSettlement::Unknown(
                    "job ownership changed after its launch acknowledgement".into(),
                );
            }
            match job.phase {
                JobPhase::Exited { code: 0 } => {}
                JobPhase::Exited { .. } if job.restart_count >= MAX_RETRIES => {}
                JobPhase::Stopped if job.runtime_absent => {}
                JobPhase::Preparing
                | JobPhase::Launching
                | JobPhase::Stopping
                | JobPhase::Exited { .. } => pending = true,
                JobPhase::Unknown | JobPhase::Stopped => {
                    return ClusterJobSettlement::Unknown(
                        "job exit or retirement is not positively confirmed".into(),
                    );
                }
            }
        }
        if pending {
            ClusterJobSettlement::Pending
        } else {
            ClusterJobSettlement::Terminal
        }
    }
}
