/// Phase 1: Filter.
///
/// Eliminates nodes that cannot run the workload. Checks readiness,
/// required labels, the capabilities the workload needs, and room.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::app::AppSpec;

use super::cluster_state::{ClusterStateCache, SchedulerNodeState};
use super::types::{NodeId, Resources, UnplacedReason};

/// The hard constraints a workload puts on the node it runs on.
///
/// Fixed-replica placement, daemon placement and the leader's check that a
/// committed placement still holds all judge a node against this, so the
/// three can't drift apart.
#[derive(Debug, Clone, Default)]
pub struct WorkloadRequirements {
    /// What one replica requests.
    pub resources: Resources,
    /// `required` placement labels: the node must carry every one.
    pub labels: BTreeMap<String, String>,
    /// The app declares egress or `allow_from` rules, which only a node that
    /// enforces them before the process starts may run.
    pub requires_egress: bool,
    /// The cluster runs internal DNS, so every placement needs a node whose
    /// resolver is ready and reachable from workloads.
    pub requires_dns: bool,
    /// The host binaries an `exec` or `script` app runs: its own (see
    /// [`host_binary`]), those of its image-less init containers (see
    /// [`host_init_binaries`]) and its pre-stop command's (see
    /// [`host_pre_stop_binary`]). Empty for a container.
    pub host_binaries: Vec<PathBuf>,
    /// The app runs an image: its own (or, with only a `command`, a root
    /// filesystem), or an init container's. Only a node whose runtime runs
    /// containers may take it (#914).
    pub requires_containers: bool,
    /// The secret providers the app's env names (`PROVIDER[name:...]`): the
    /// node must run every one (#1123).
    pub secret_providers: std::collections::BTreeSet<crate::sesame::secret_provider::ProviderName>,
    /// The runtime knobs the app asks for in `[app.x.runtime]`: the node's
    /// advertised allowlist must admit every one (#1122).
    pub runtime_knobs: crate::grill::knobs::RuntimeKnobs,
}

impl WorkloadRequirements {
    /// The requirements of `spec`'s replicas, in a cluster that does or
    /// doesn't require internal DNS.
    pub fn for_app(spec: &AppSpec, requires_dns: bool) -> Self {
        Self {
            resources: crate::meat::admission::app_requests(spec),
            labels: spec
                .placement
                .as_ref()
                .map(|p| super::scheduler::parse_label_list(&p.required))
                .unwrap_or_default(),
            requires_egress: spec.requires_network_policy(),
            requires_dns,
            host_binaries: host_binary(spec)
                .into_iter()
                .chain(host_init_binaries(spec))
                .chain(host_pre_stop_binary(spec))
                .map(Path::to_path_buf)
                .collect(),
            requires_containers: (spec.exec.is_none() && spec.script.is_none())
                || spec.init.iter().any(|init| init.image.is_some()),
            secret_providers: spec
                .env
                .values()
                .filter_map(|value| value.provider_reference())
                .map(|reference| reference.provider().clone())
                .collect(),
            runtime_knobs: spec.runtime.clone().unwrap_or_default(),
        }
    }
}

/// The host binary a process workload runs: an `exec` app's path, or the
/// script interpreter for a `script` app. `None` for a container.
///
/// A node refuses an `exec` app whose path isn't in its
/// `[process_workloads] allowed_binaries`, and a `script` app unless
/// `/bin/sh` is (`Supervisor::admit_process_workload`). Placement asks the
/// same question here, so app validation (#701) and any other caller that
/// needs it should too.
pub fn host_binary(spec: &AppSpec) -> Option<&Path> {
    spec.exec.as_deref().or_else(|| {
        spec.script
            .as_ref()
            .map(|_| Path::new(crate::grill::process_workload::SCRIPT_INTERPRETER))
    })
}

/// The host binaries a process workload's init containers run: the first
/// word of each image-less init's command. An `exec` or `script` app's init
/// that names no image runs on the host like its app (#935), so the node
/// must allow its binary too. Empty for a container, whose inits run in an
/// image. An init with no command names no binary; the runtime refuses it.
pub fn host_init_binaries(spec: &AppSpec) -> impl Iterator<Item = &Path> {
    let host = host_binary(spec).is_some();
    spec.init
        .iter()
        .filter(move |init| host && init.image.is_none())
        .filter_map(|init| init.command.first())
        .map(Path::new)
}

/// The host binary a process workload's `pre_stop` runs (#1121). Like an
/// image-less init, it runs on the host beside its app, so the node must
/// allow it too. `None` for a container, whose pre-stop runs in its image.
pub fn host_pre_stop_binary(spec: &AppSpec) -> Option<&Path> {
    host_binary(spec)?;
    spec.pre_stop.first().map(Path::new)
}

/// Whether `node` can run a workload with these requirements, room and
/// labels aside: it's ready, runs containers when the workload is one, and
/// can enforce egress, resolve internal DNS, run the host binary, call the
/// secret providers and apply the runtime knobs when the workload needs
/// them.
pub fn node_can_run(node: &SchedulerNodeState, requirements: &WorkloadRequirements) -> bool {
    node.ready
        && runs_its_kind(node, requirements)
        && (!requirements.requires_egress || node.capabilities.egress.can_enforce_allowlist())
        && (!requirements.requires_dns || node.capabilities.dns.can_resolve_internal())
        && requirements
            .host_binaries
            .iter()
            .all(|binary| node.capabilities.process.runs(binary))
        && requirements
            .secret_providers
            .is_subset(&node.capabilities.secret_providers)
        && node
            .capabilities
            .runtime_knobs
            .admits(&requirements.runtime_knobs)
}

/// Whether `node` runs this kind of workload at all: a container needs a
/// container runtime (#914).
fn runs_its_kind(node: &SchedulerNodeState, requirements: &WorkloadRequirements) -> bool {
    !requirements.requires_containers || node.capabilities.containers
}

/// Filter the cluster to nodes eligible for placing this workload.
///
/// A node passes the filter if:
/// 1. It is ready: it has a fresh readiness report, its egress enforcement
///    isn't degraded, and it hasn't been retired. Drain and cordon don't
///    exist yet (#656).
/// 2. It matches all required placement labels.
/// 3. It runs containers if the workload is one, can enforce any security
///    capability the workload requires, and runs its host binary if it has
///    one.
/// 4. It has sufficient allocatable resources after current allocations.
pub fn filter_nodes(
    requirements: &WorkloadRequirements,
    cluster: &ClusterStateCache,
) -> Vec<NodeId> {
    cluster
        .nodes()
        .filter(|node| {
            node_can_run(node, requirements)
                && node.matches_labels(&requirements.labels)
                && node.can_fit(&requirements.resources)
        })
        .map(|node| node.node_id.clone())
        .collect()
}

/// Why [`filter_nodes`] found no node: the first constraint, in filter
/// order, that no node meets.
pub fn unplaced_reason(
    requirements: &WorkloadRequirements,
    cluster: &ClusterStateCache,
) -> UnplacedReason {
    let ready: Vec<&SchedulerNodeState> = cluster.nodes().filter(|node| node.ready).collect();
    if ready.is_empty() {
        return UnplacedReason::NoReadyNode;
    }
    let labelled: Vec<&SchedulerNodeState> = ready
        .into_iter()
        .filter(|node| node.matches_labels(&requirements.labels))
        .collect();
    if labelled.is_empty() {
        return UnplacedReason::NoLabelMatch;
    }
    if !labelled
        .iter()
        .any(|node| runs_its_kind(node, requirements))
    {
        return UnplacedReason::NoContainerRuntime;
    }
    if !labelled.iter().any(|node| node_can_run(node, requirements)) {
        return UnplacedReason::NoCapableNode;
    }
    UnplacedReason::NoRoom
}

/// Cordon nodes that are mid-binary-upgrade (Phase 14): mark them not
/// ready so `filter_nodes` skips them. Call after populating the cache,
/// with `active_upgrade` from the Raft `DesiredState`.
///
/// The leader's orchestration pass applies this to its populated cache before
/// scheduling, using the active upgrade from replicated desired state.
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
            capabilities: crate::meat::cluster_state::NodeCapabilities::container_runtime(),
            app_replicas: Default::default(),
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
            &WorkloadRequirements {
                resources: Resources::new(500, 1024, 0),
                labels: BTreeMap::new(),
                requires_egress: false,
                requires_dns: false,
                host_binaries: Vec::new(),
                requires_containers: false,
                secret_providers: Default::default(),
                runtime_knobs: Default::default(),
            },
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
            &WorkloadRequirements {
                resources: Resources::new(100, 100, 0),
                labels: required.clone(),
                requires_egress: false,
                requires_dns: false,
                host_binaries: Vec::new(),
                requires_containers: false,
                secret_providers: Default::default(),
                runtime_knobs: Default::default(),
            },
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
            &WorkloadRequirements {
                resources: Resources::new(100, 100, 0),
                labels: BTreeMap::new(),
                requires_egress: false,
                requires_dns: false,
                host_binaries: Vec::new(),
                requires_containers: false,
                secret_providers: Default::default(),
                runtime_knobs: Default::default(),
            },
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
            &WorkloadRequirements {
                resources: Resources::new(100, 100, 0),
                labels: BTreeMap::new(),
                requires_egress: false,
                requires_dns: false,
                host_binaries: Vec::new(),
                requires_containers: false,
                secret_providers: Default::default(),
                runtime_knobs: Default::default(),
            },
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
            &WorkloadRequirements {
                resources: Resources::new(100, 100, 0),
                labels: BTreeMap::new(),
                requires_egress: false,
                requires_dns: true,
                host_binaries: Vec::new(),
                requires_containers: false,
                secret_providers: Default::default(),
                runtime_knobs: Default::default(),
            },
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
            platform: None,
            from_version: None,
            phase,
            since: None,
            directive_retry: None,
        };
        let upgrade = ClusterUpgradeState {
            upgrade_id: "up-1".to_string(),
            target_version: "v0.2.0".parse().unwrap(),
            binaries: Vec::new(),
            parallel: 1,
            direction: UpgradeDirection::Upgrade,
            phase: ClusterUpgradePhase::UpgradingWorkers,
            registry_address: String::new(),
            allow_downgrade: false,
            nodes: vec![
                record("n1", NodeUpgradePhase::Directed),
                record("n2", NodeUpgradePhase::Pending),
            ],
        };

        apply_upgrade_cordon(&mut cluster, Some(&upgrade));

        let result = filter_nodes(
            &WorkloadRequirements {
                resources: Resources::new(100, 100, 0),
                labels: BTreeMap::new(),
                requires_egress: false,
                requires_dns: false,
                host_binaries: Vec::new(),
                requires_containers: false,
                secret_providers: Default::default(),
                runtime_knobs: Default::default(),
            },
            &cluster,
        );
        // n1 is mid-upgrade (Directed) and cordoned; n2 is merely Pending
        // and keeps taking work until its turn actually comes.
        assert_eq!(result, vec![NodeId::new("n2")]);
    }

    fn running(name: &str, enabled: bool, allowed: &[&str]) -> SchedulerNodeState {
        let mut node = node_state(name, 2000, 4096, BTreeMap::new(), true);
        node.capabilities.process = crate::meat::cluster_state::ProcessWorkloadCapability {
            enabled,
            allowed_binaries: allowed.iter().map(PathBuf::from).collect(),
        };
        node
    }

    /// #1123: an app whose env names a secret provider lands only on nodes
    /// that run that provider; an app without one lands anywhere.
    #[test]
    fn an_app_naming_a_secret_provider_skips_nodes_without_it() {
        let provider = |name: &str| {
            crate::sesame::secret_provider::ProviderName::try_from(name.to_string()).unwrap()
        };
        let mut cluster = ClusterStateCache::new();
        let mut vault = node_state("vault", 2000, 4096, BTreeMap::new(), true);
        vault.capabilities.secret_providers = [provider("vault")].into();
        cluster.set_node(vault);
        let mut aws = node_state("aws", 2000, 4096, BTreeMap::new(), true);
        aws.capabilities.secret_providers = [provider("aws")].into();
        cluster.set_node(aws);
        cluster.set_node(node_state("plain", 2000, 4096, BTreeMap::new(), true));

        let spec: AppSpec = toml::from_str(
            "image = \"api:1\"\n[env]\nDB_PASSWORD = \"PROVIDER[vault:secret/db]\"\nMODE = \"x\"\n",
        )
        .unwrap();
        let requirements = WorkloadRequirements::for_app(&spec, false);
        assert_eq!(
            filter_nodes(&requirements, &cluster),
            vec![NodeId::new("vault")]
        );

        let mut cluster_without = ClusterStateCache::new();
        cluster_without.set_node(node_state("plain", 2000, 4096, BTreeMap::new(), true));
        assert_eq!(
            unplaced_reason(&requirements, &cluster_without),
            UnplacedReason::NoCapableNode
        );

        let plain: AppSpec = toml::from_str("image = \"api:1\"\n").unwrap();
        let anywhere = filter_nodes(&WorkloadRequirements::for_app(&plain, false), &cluster);
        assert_eq!(anywhere.len(), 3);
    }

    /// #1122: an app that asks for runtime knobs lands only on a node whose
    /// advertised `[runtime]` allowlist admits every one of them. A node
    /// whose runtime can't apply knobs advertises an empty allowlist.
    #[test]
    fn an_app_with_runtime_knobs_lands_only_where_the_node_allows_them() {
        let allowing = |toml_source: &str| {
            crate::config::NodeConfig::parse(toml_source)
                .unwrap()
                .runtime
                .knob_allowlist()
        };
        let mut cluster = ClusterStateCache::new();
        let mut fuse = node_state("fuse", 2000, 4096, BTreeMap::new(), true);
        fuse.capabilities.runtime_knobs = allowing(
            "[runtime]\nallowed_capabilities = [\"SYS_ADMIN\"]\nallowed_devices = [\"/dev/fuse\"]\n",
        );
        cluster.set_node(fuse);
        let mut network = node_state("network", 2000, 4096, BTreeMap::new(), true);
        network.capabilities.runtime_knobs = allowing(
            "[runtime]\nallowed_capabilities = [\"NET_ADMIN\"]\nallowed_sysctls = [\"net.*\"]\n",
        );
        cluster.set_node(network);
        cluster.set_node(node_state("plain", 2000, 4096, BTreeMap::new(), true));

        let tuned: AppSpec = toml::from_str(
            "image = \"lb:1\"\n[runtime]\ncapabilities = [\"NET_ADMIN\"]\n\
             sysctls = { \"net.core.somaxconn\" = \"4096\" }\n",
        )
        .unwrap();
        assert_eq!(
            filter_nodes(&WorkloadRequirements::for_app(&tuned, false), &cluster),
            vec![NodeId::new("network")]
        );

        // Each knob must be allowed: SYS_ADMIN alone isn't enough for fuse
        // plus a sysctl the fuse node doesn't list.
        let mixed: AppSpec = toml::from_str(
            "image = \"fs:1\"\n[runtime]\ncapabilities = [\"SYS_ADMIN\"]\n\
             devices = [{ path = \"/dev/fuse\" }]\nsysctls = { \"net.core.somaxconn\" = \"1\" }\n",
        )
        .unwrap();
        let requirements = WorkloadRequirements::for_app(&mixed, false);
        assert!(filter_nodes(&requirements, &cluster).is_empty());
        assert_eq!(
            unplaced_reason(&requirements, &cluster),
            UnplacedReason::NoCapableNode
        );

        let plain: AppSpec = toml::from_str("image = \"api:1\"\n[runtime]\n").unwrap();
        assert_eq!(
            filter_nodes(&WorkloadRequirements::for_app(&plain, false), &cluster).len(),
            3
        );
    }

    /// #702: nodes with different `[process_workloads]` policies. An exec
    /// app lands only where the node will start it; anywhere else the node
    /// refuses it and the placement used to hold there for good.
    #[test]
    fn exec_app_skips_a_node_whose_allowlist_lacks_its_binary() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(running("python", true, &["/usr/bin/python3"]));
        cluster.set_node(running("ruby", true, &["/usr/bin/ruby"]));
        cluster.set_node(running("isolated", false, &["/usr/bin/python3"]));
        cluster.set_node(running("shell", true, &["/bin/sh"]));
        let spec: AppSpec = toml::from_str(r#"exec = "/usr/bin/python3""#).unwrap();

        let result = filter_nodes(&WorkloadRequirements::for_app(&spec, false), &cluster);

        assert_eq!(result, vec![NodeId::new("python")]);

        // A script runs the interpreter, so only `/bin/sh` admits it.
        let script: AppSpec = toml::from_str(r#"script = "echo hi""#).unwrap();
        let result = filter_nodes(&WorkloadRequirements::for_app(&script, false), &cluster);
        assert_eq!(result, vec![NodeId::new("shell")]);

        // A container runs anywhere, whatever the allowlist says.
        let container: AppSpec = toml::from_str(r#"image = "web:1""#).unwrap();
        let result = filter_nodes(&WorkloadRequirements::for_app(&container, false), &cluster);
        assert_eq!(result.len(), 4);
    }

    /// #935: a host app's image-less init container runs its command on the
    /// host, so the app lands only where the node allows that binary too,
    /// or the node would refuse it after placement.
    #[test]
    fn a_host_apps_init_binary_must_be_allowlisted_for_placement() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(running("app-only", true, &["/usr/bin/python3"]));
        cluster.set_node(running(
            "both",
            true,
            &["/usr/bin/python3", "/usr/bin/migrate"],
        ));
        let spec: AppSpec = toml::from_str(
            "exec = \"/usr/bin/python3\"\n[[init]]\ncommand = [\"/usr/bin/migrate\"]",
        )
        .unwrap();

        let result = filter_nodes(&WorkloadRequirements::for_app(&spec, false), &cluster);

        assert_eq!(result, vec![NodeId::new("both")]);
    }

    #[test]
    fn an_empty_cluster_says_no_node_is_ready() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("down", 2000, 4096, BTreeMap::new(), false));
        let needs = WorkloadRequirements::default();

        assert_eq!(
            unplaced_reason(&needs, &cluster),
            UnplacedReason::NoReadyNode
        );
    }

    #[test]
    fn the_reason_names_the_first_constraint_no_node_meets() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(running("ruby", true, &["/usr/bin/ruby"]));
        let mut needs = WorkloadRequirements {
            labels: labels(&[("zone", "east")]),
            host_binaries: vec![PathBuf::from("/usr/bin/python3")],
            resources: Resources::new(9000, 0, 0),
            ..Default::default()
        };
        assert_eq!(
            unplaced_reason(&needs, &cluster),
            UnplacedReason::NoLabelMatch
        );

        needs.labels.clear();
        assert_eq!(
            unplaced_reason(&needs, &cluster),
            UnplacedReason::NoCapableNode
        );

        needs.host_binaries.clear();
        assert_eq!(unplaced_reason(&needs, &cluster), UnplacedReason::NoRoom);
        assert!(filter_nodes(&needs, &cluster).is_empty());
    }

    /// `node` on `--runtime process`: no container runtime.
    fn process_only(mut node: SchedulerNodeState) -> SchedulerNodeState {
        node.capabilities.containers = false;
        node
    }

    /// #914: an app that names an image needs a container runtime, and a
    /// node on `--runtime process` refuses it. Placement keeps it off such a
    /// node, so it lands where it can run instead of failing after placement.
    #[test]
    fn image_app_skips_a_node_without_a_container_runtime() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(running("runc", true, &["/bin/sh"]));
        cluster.set_node(process_only(running("process", true, &["/bin/sh"])));

        let image: AppSpec = toml::from_str(r#"image = "web:1""#).unwrap();
        let result = filter_nodes(&WorkloadRequirements::for_app(&image, false), &cluster);
        assert_eq!(result, vec![NodeId::new("runc")]);

        // So is an init container's own image, even before an exec app.
        let init: AppSpec = toml::from_str(
            "exec = \"/bin/sh\"\n[[init]]\nimage = \"migrate:2\"\ncommand = [\"migrate\"]",
        )
        .unwrap();
        let result = filter_nodes(&WorkloadRequirements::for_app(&init, false), &cluster);
        assert_eq!(result, vec![NodeId::new("runc")]);

        // A host workload runs on either.
        let script: AppSpec = toml::from_str(r#"script = "echo hi""#).unwrap();
        let mut result = filter_nodes(&WorkloadRequirements::for_app(&script, false), &cluster);
        result.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(result, vec![NodeId::new("process"), NodeId::new("runc")]);
    }

    #[test]
    fn an_image_app_with_only_process_nodes_is_unplaced_for_want_of_a_container_runtime() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(process_only(running("process", true, &["/bin/sh"])));
        let image: AppSpec = toml::from_str(r#"image = "web:1""#).unwrap();

        let reason = unplaced_reason(&WorkloadRequirements::for_app(&image, false), &cluster);

        assert_eq!(reason, UnplacedReason::NoContainerRuntime);
        assert_eq!(
            reason.to_string(),
            "no ready node has a container runtime to run its image"
        );
    }

    /// #1121: a host app's pre-stop command runs on the host, so placement
    /// needs a node that allows its program too. A container's runs in its
    /// image and asks nothing of the node.
    #[test]
    fn a_host_apps_pre_stop_program_is_a_host_binary_it_needs() {
        let host: AppSpec =
            toml::from_str("exec = \"/bin/sleep\"\npre_stop = [\"/bin/echo\", \"bye\"]").unwrap();
        assert_eq!(
            WorkloadRequirements::for_app(&host, false).host_binaries,
            vec![PathBuf::from("/bin/sleep"), PathBuf::from("/bin/echo")]
        );
        let container: AppSpec =
            toml::from_str("image = \"web:1\"\npre_stop = [\"/bin/echo\"]").unwrap();
        assert!(
            WorkloadRequirements::for_app(&container, false)
                .host_binaries
                .is_empty()
        );
    }
}
