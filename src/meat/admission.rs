//! Shared commitments reconstructed before either app or batch admission.

use crate::config::{app::AppSpec, job::JobSpec};
use crate::council::types::DesiredState;
use crate::meat::{AppId, ClusterStateCache, NodeId, Resources};
use crate::reporting::types::StateReport;

/// App requests use minimum values; an omitted resource requests zero.
pub fn app_requests(spec: &AppSpec) -> Resources {
    Resources::new(
        spec.cpu.as_ref().map_or(0, |r| r.request),
        spec.memory.as_ref().map_or(0, |r| r.request),
        spec.gpu.unwrap_or(0),
    )
}

/// Jobs share app CPU/memory defaults and do not declare GPUs.
pub fn job_requests(spec: &JobSpec) -> Resources {
    Resources::new(
        spec.cpu.as_ref().map_or(0, |r| r.request),
        spec.memory.as_ref().map_or(0, |r| r.request),
        0,
    )
}

/// Requests not already represented by this node's exact reported instances.
/// A missing report never releases an assigned execution's reservation.
pub fn unreported_commitments(
    desired: &DesiredState,
    node: &NodeId,
    report: &StateReport,
) -> Resources {
    let mut reported = std::collections::HashMap::<(String, String, u32), Resources>::new();
    for instance in &report.running_apps {
        let key = (
            instance.app_name.clone(),
            instance.namespace.clone(),
            instance.instance_id,
        );
        let requests = Resources::new(
            u64::from(instance.resource_usage.cpu_millicores),
            u64::from(instance.resource_usage.memory_mb) * 1024 * 1024,
            0,
        );
        let total = reported.entry(key).or_default();
        *total = total.saturating_add(&requests);
    }
    let mut missing = Resources::default();
    let mut seen = std::collections::HashSet::new();
    let mut add = |name: &str, namespace: &str, ordinal: u32, request: Resources| {
        if !seen.insert((name.to_string(), namespace.to_string(), ordinal)) {
            return;
        }
        let reported = reported
            .get(&(name.to_string(), namespace.to_string(), ordinal))
            .copied()
            .unwrap_or_default();
        missing = missing.saturating_add(&request.saturating_sub(&reported));
    };
    for (app, placements) in &desired.scheduling {
        for placement in placements.iter().filter(|p| &p.node_id == node) {
            add(
                &app.name,
                &app.namespace,
                placement.ordinal,
                placement.resources,
            );
        }
    }
    for (_, batch) in &desired.batch_state.batches {
        for job in batch
            .jobs
            .iter()
            .filter(|job| job.node.as_ref() == Some(node) && !job.status.is_terminal())
        {
            add(&job.execution_name, &job.namespace, 0, job.resources);
        }
    }
    missing
}

/// Reconstruct both app and batch commitments using exact reported ordinals.
/// A partial request credits only that amount, never the whole placement.
pub fn reserve_commitments(cache: &mut ClusterStateCache, desired: &DesiredState) {
    for (app, placements) in &desired.scheduling {
        for placement in placements {
            cache.reserve_committed_instance(
                &placement.node_id,
                app,
                placement.ordinal,
                placement.resources,
            );
        }
    }
    for (_, batch) in &desired.batch_state.batches {
        for job in batch.jobs.iter().filter(|job| !job.status.is_terminal()) {
            if let Some(node) = &job.node {
                cache.reserve_committed_instance(
                    node,
                    &AppId::new(&job.execution_name, &job.namespace),
                    0,
                    job.resources,
                );
            }
        }
    }
}
