//! Committed app placements reconstructed before a scheduling pass.
//!
//! Job work (task attempts and reusable executors) never appears here: each
//! node reports what it holds in its execution budget, and
//! `build_cluster_cache` adds that to the node's allocated total.

use crate::config::app::AppSpec;
use crate::council::types::DesiredState;
use crate::meat::{ClusterStateCache, Resources};

/// What one replica of an app requests: its own requests plus each of its
/// sidecars' (#1120). Requests use minimum values; an omitted resource
/// requests zero.
pub fn app_requests(spec: &AppSpec) -> Resources {
    let request =
        |range: Option<&crate::config::types::ResourceRange>| range.map_or(0, |r| r.request);
    let cpu = spec
        .sidecar
        .iter()
        .map(|sidecar| request(sidecar.cpu.as_ref()))
        .fold(request(spec.cpu.as_ref()), u64::saturating_add);
    let memory = spec
        .sidecar
        .iter()
        .map(|sidecar| request(sidecar.memory.as_ref()))
        .fold(request(spec.memory.as_ref()), u64::saturating_add);
    Resources::new(cpu, memory, spec.gpu.unwrap_or(0))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #1120: a sidecar's requests add to its app's, so placement, quotas
    /// and a node's own admission reserve room for both.
    #[test]
    fn an_apps_requests_include_its_sidecars() {
        let spec: AppSpec = toml::from_str(
            "image = \"api:1\"\ncpu = \"500m-1\"\nmemory = \"256Mi\"\n\
             [[sidecar]]\nname = \"proxy\"\ncpu = \"100m-200m\"\nmemory = \"64Mi-128Mi\"\n\
             [[sidecar]]\nname = \"logs\"\nmemory = \"32Mi\"\n",
        )
        .unwrap();
        let requests = app_requests(&spec);
        assert_eq!(requests.cpu_millicores, 600);
        assert_eq!(requests.memory_bytes, (256 + 64 + 32) * 1024 * 1024);
    }

    #[test]
    fn an_app_without_sidecars_requests_only_its_own() {
        let spec: AppSpec = toml::from_str("image = \"api:1\"\ncpu = \"250m\"\n").unwrap();
        assert_eq!(app_requests(&spec), Resources::new(250, 0, 0));
    }
}
