/// Phase 1: Filter.
///
/// Eliminates nodes that cannot run the workload. Checks capacity,
/// required labels, and readiness.
use std::collections::BTreeMap;

use super::cluster_state::ClusterStateCache;
use super::types::{NodeId, Resources};

/// Filter the cluster to nodes eligible for placing this workload.
///
/// A node passes the filter if:
/// 1. It is ready (not unknown, draining, or cordoned).
/// 2. It has sufficient allocatable resources after current allocations.
/// 3. It matches all required placement labels.
/// 4. It can enforce any security capability the workload requires.
pub fn filter_nodes(
    resources: &Resources,
    required_labels: &BTreeMap<String, String>,
    requires_egress: bool,
    requires_dns: bool,
    cluster: &ClusterStateCache,
) -> Vec<NodeId> {
    cluster
        .nodes()
        .filter(|node| {
            node.ready
                && node.can_fit(resources)
                && node.matches_labels(required_labels)
                && (!requires_egress || node.capabilities.egress.can_enforce_allowlist())
                && (!requires_dns || node.capabilities.dns.can_resolve_internal())
        })
        .map(|node| node.node_id.clone())
        .collect()
}

/// Cordon nodes that are mid-binary-upgrade (Phase 14): mark them not
/// ready so `filter_nodes` skips them. Call after populating the cache,
/// with `active_upgrade` from the Raft `DesiredState`.
///
/// TODO(wiring): call from the leader's scheduling path once the binary
/// populates a `ClusterStateCache` from gossip + reports.
pub fn apply_upgrade_cordon(
    cluster: &mut ClusterStateCache,
    upgrade: Option<&crate::upgrade::types::ClusterUpgradeState>,
) {
    let Some(upgrade) = upgrade else { return };
    let cordoned: Vec<_> = cluster
        .nodes()
        .filter(|node| upgrade.is_node_cordoned(&node.node_id.0))
        .cloned()
        .collect();
    for mut node in cordoned {
        node.ready = false;
        cluster.set_node(node);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::meat::cluster_state::SchedulerNodeState;

    fn node_state(
        name: &str,
        cpu: u64,
        mem: u64,
        labels: BTreeMap<String, String>,
        ready: bool,
    ) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, mem, 0),
            allocated: Resources::default(),
            labels,
            ready,
            capabilities: Default::default(),
            running_apps: HashSet::new(),
            uptime_secs: 86400,
            cached_images: HashSet::new(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn filters_out_nodes_without_capacity() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("big", 2000, 4096, BTreeMap::new(), true));
        cluster.set_node(node_state("small", 100, 256, BTreeMap::new(), true));

        let result = filter_nodes(
            &Resources::new(500, 1024, 0),
            &BTreeMap::new(),
            false,
            false,
            &cluster,
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], NodeId::new("big"));
    }

    #[test]
    fn filters_out_nodes_missing_required_labels() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state(
            "east",
            2000,
            4096,
            labels(&[("zone", "us-east")]),
            true,
        ));
        cluster.set_node(node_state(
            "west",
            2000,
            4096,
            labels(&[("zone", "us-west")]),
            true,
        ));

        let required = labels(&[("zone", "us-east")]);
        let result = filter_nodes(
            &Resources::new(100, 100, 0),
            &required,
            false,
            false,
            &cluster,
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], NodeId::new("east"));
    }

    #[test]
    fn filters_out_non_ready_nodes() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("ready", 2000, 4096, BTreeMap::new(), true));
        cluster.set_node(node_state("not-ready", 2000, 4096, BTreeMap::new(), false));

        let result = filter_nodes(
            &Resources::new(100, 100, 0),
            &BTreeMap::new(),
            false,
            false,
            &cluster,
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], NodeId::new("ready"));
    }

    #[test]
    fn returns_all_eligible_nodes() {
        let mut cluster = ClusterStateCache::new();
        for i in 0..5 {
            cluster.set_node(node_state(
                &format!("n{i}"),
                2000,
                4096,
                BTreeMap::new(),
                true,
            ));
        }

        let result = filter_nodes(
            &Resources::new(100, 100, 0),
            &BTreeMap::new(),
            false,
            false,
            &cluster,
        );
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn dns_enabled_placement_requires_ready_workload_reachable_resolver() {
        let mut cluster = ClusterStateCache::new();
        let mut capable = node_state("dns-ready", 2000, 4096, BTreeMap::new(), true);
        capable.capabilities.dns = crate::onion::dns::DnsCapability {
            enabled: true,
            ready: true,
            ipv4: true,
            ipv6: false,
            workload_reachable: true,
        };
        cluster.set_node(capable);

        let mut unready = node_state("dns-unready", 2000, 4096, BTreeMap::new(), true);
        unready.capabilities.dns = crate::onion::dns::DnsCapability {
            enabled: true,
            ready: false,
            ipv4: true,
            ipv6: false,
            workload_reachable: true,
        };
        cluster.set_node(unready);

        let result = filter_nodes(
            &Resources::new(100, 100, 0),
            &BTreeMap::new(),
            false,
            true,
            &cluster,
        );
        assert_eq!(result, vec![NodeId::new("dns-ready")]);
    }

    #[test]
    fn scheduler_skips_nodes_mid_upgrade() {
        use crate::upgrade::types::{
            ClusterUpgradePhase, ClusterUpgradeState, NodeRole, NodeUpgradePhase,
            NodeUpgradeRecord, UpgradeDirection,
        };

        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("n1", 2000, 4096, BTreeMap::new(), true));
        cluster.set_node(node_state("n2", 2000, 4096, BTreeMap::new(), true));

        let record = |node: &str, phase: NodeUpgradePhase| NodeUpgradeRecord {
            node_id: node.to_string(),
            address: "127.0.0.1:9117".to_string(),
            role: NodeRole::Worker,
            from_version: None,
            phase,
            since: None,
        };
        let upgrade = ClusterUpgradeState {
            upgrade_id: "up-1".to_string(),
            target_version: "v0.2.0".parse().unwrap(),
            binary_sha256: String::new(),
            embedded_signature: String::new(),
            external_signature: None,
            parallel: 1,
            direction: UpgradeDirection::Upgrade,
            phase: ClusterUpgradePhase::UpgradingWorkers,
            registry_address: String::new(),
            nodes: vec![
                record("n1", NodeUpgradePhase::Directed),
                record("n2", NodeUpgradePhase::Pending),
            ],
        };

        apply_upgrade_cordon(&mut cluster, Some(&upgrade));

        let result = filter_nodes(
            &Resources::new(100, 100, 0),
            &BTreeMap::new(),
            false,
            false,
            &cluster,
        );
        // n1 is mid-upgrade (Directed) and cordoned; n2 is merely Pending
        // and keeps taking work until its turn actually comes.
        assert_eq!(result, vec![NodeId::new("n2")]);
    }
}
