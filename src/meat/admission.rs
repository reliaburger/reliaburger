//! Committed app placements reconstructed before a scheduling pass.
//!
//! Job work (task attempts and reusable executors) never appears here: each
//! node reports what it holds in its execution budget, and
//! `build_cluster_cache` adds that to the node's allocated total.

use crate::config::app::AppSpec;
use crate::council::types::DesiredState;
use crate::meat::{ClusterStateCache, Resources};

/// App requests use minimum values; an omitted resource requests zero.
pub fn app_requests(spec: &AppSpec) -> Resources {
    Resources::new(
        spec.cpu.as_ref().map_or(0, |r| r.request),
        spec.memory.as_ref().map_or(0, |r| r.request),
        spec.gpu.unwrap_or(0),
    )
}

/// Reserve every committed placement using exact reported ordinals.
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
}
