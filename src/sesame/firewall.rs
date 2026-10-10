//! eBPF firewall map wiring.
//!
//! Populates the `firewall_map` and `cgroup_namespace_map` BPF maps
//! from app configuration `allow_from` rules. The eBPF connect hook
//! (already implemented in Onion) checks these maps on every `connect()`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::onion::types::{
    DestinationKey, DestinationValue, FirewallKey, FirewallValue, NAMESPACE_CONTESTED, ServiceEntry,
};

/// The action value for ALLOW in the firewall map.
pub const FIREWALL_ALLOW: u32 = 1;
/// The action value for DENY in the firewall map.
pub const FIREWALL_DENY: u32 = 0;

/// A resolved firewall rule ready to be written to the BPF map.
#[derive(Debug, Clone)]
pub struct ResolvedFirewallRule {
    /// Source cgroup ID (the connecting process).
    pub src_cgroup_id: u64,
    /// Destination app ID (the target service).
    pub dst_app_id: u32,
    /// Whether to allow or deny.
    pub action: u32,
}

/// Cgroup-to-namespace mapping for the `cgroup_namespace_map` BPF map.
#[derive(Debug, Clone)]
pub struct CgroupNamespaceEntry {
    /// The cgroup ID of a running container.
    pub cgroup_id: u64,
    /// The namespace ID the container belongs to.
    pub namespace_id: u32,
}

/// Live kernel values used by `relish path` to mirror the connect hook's
/// namespace decision without pretending declared policy is kernel truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveFirewallState {
    /// Source namespace id from `cgroup_namespace_map`, or `None` when the
    /// source cgroup would bypass namespace isolation.
    pub source_namespace_id: Option<u32>,
    /// Explicit cross-namespace action from `firewall_map`.
    pub action: Option<u32>,
}

/// Resolve firewall rules from app configs and running instance state.
///
/// Given the current service map entries and cgroup IDs for each app,
/// produces a list of `FirewallKey → FirewallValue` entries for the
/// BPF map. The connect hook denies connections that aren't explicitly
/// in this map.
///
/// Logic:
/// - If `firewall_allow_from` is `None`, all apps in the same namespace
///   are allowed (default namespace isolation).
/// - If `firewall_allow_from` is `Some(list)`, only the named apps are
///   allowed. Names can be cross-namespace using `namespace/app` format.
pub fn resolve_firewall_rules(
    services: &[ServiceEntry],
    cgroup_ids: &HashMap<(String, String), Vec<u64>>,
) -> Vec<ResolvedFirewallRule> {
    let destinations: Vec<IsolatedDestination<'_>> =
        services.iter().map(IsolatedDestination::from).collect();
    resolve_destination_rules(&destinations, cgroup_ids)
}

/// [`resolve_firewall_rules`] for any isolated destination, services and
/// jobs alike.
pub fn resolve_destination_rules(
    destinations: &[IsolatedDestination<'_>],
    cgroup_ids: &HashMap<(String, String), Vec<u64>>,
) -> Vec<ResolvedFirewallRule> {
    let mut rules = Vec::new();
    for destination in destinations {
        let grant = |rules: &mut Vec<ResolvedFirewallRule>, cgroups: &[u64]| {
            rules.extend(cgroups.iter().map(|&cg| ResolvedFirewallRule {
                src_cgroup_id: cg,
                dst_app_id: destination.app_id,
                action: FIREWALL_ALLOW,
            }));
        };
        match destination.allow_from {
            None => {
                // Default: allow all apps in the same namespace
                for ((namespace, app), cgroups) in cgroup_ids {
                    if namespace == destination.namespace && app != destination.name {
                        grant(&mut rules, cgroups);
                    }
                }
            }
            Some(allow_list) => {
                // Support "namespace/app" or just "app" (same namespace)
                for allowed_name in allow_list {
                    let (target_ns, target_app) = allowed_name
                        .split_once('/')
                        .unwrap_or((destination.namespace, allowed_name.as_str()));
                    if let Some(cgroups) =
                        cgroup_ids.get(&(target_ns.to_string(), target_app.to_string()))
                    {
                        grant(&mut rules, cgroups);
                    }
                }
            }
        }
    }
    rules
}

/// A destination namespace isolation protects, and who may reach it from
/// other namespaces. Services and jobs both become one.
#[derive(Debug, Clone, Copy)]
pub struct IsolatedDestination<'a> {
    /// Namespace that owns the destination.
    pub namespace: &'a str,
    /// App or job name.
    pub name: &'a str,
    /// Identity `firewall_map` grants name.
    pub app_id: u32,
    /// `allow_from` sources; `None` keeps namespace-default isolation.
    pub allow_from: Option<&'a [String]>,
}

impl<'a> From<&'a ServiceEntry> for IsolatedDestination<'a> {
    fn from(service: &'a ServiceEntry) -> Self {
        Self {
            namespace: &service.namespace,
            name: &service.app_name,
            app_id: service.app_id,
            allow_from: service.firewall_allow_from.as_deref(),
        }
    }
}

/// The `firewall_map` identity of a workload that publishes no service, such
/// as a job. Service identities are VIPs (`127.128.0.0/16` as a host-order
/// `u32`, below `0x8000_0000`), so setting the top bit keeps the two apart.
pub fn workload_app_id(namespace: &str, name: &str) -> u32 {
    0x8000_0000 | (crate::onion::vip::name_to_id(&format!("{namespace}/{name}")) & 0x7fff_ffff)
}

/// A workload on this node with its own (container) address, which belongs
/// to its namespace on every port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWorkload {
    /// The workload's own address.
    pub address: std::net::Ipv4Addr,
    /// Owning namespace.
    pub namespace: String,
    /// App or job name; for a delegated task, its runtime instance.
    pub name: String,
    /// The owner's `firewall_map` identity: its service's, when it publishes
    /// one, so one `allow_from` grant covers its VIP and real addresses.
    pub app_id: u32,
}

/// The forward-path plan for `crate::firewall::isolation`: local workload
/// addresses by namespace, and the address pairs `allow_from` opens.
pub fn isolation_plan(
    workloads: &[LocalWorkload],
    destinations: &[IsolatedDestination<'_>],
) -> crate::firewall::isolation::IsolationPlan {
    let mut plan = crate::firewall::isolation::IsolationPlan::default();
    for workload in workloads {
        plan.namespaces
            .entry(crate::onion::vip::name_to_id(&workload.namespace))
            .or_default()
            .insert(workload.address);
    }
    for destination in destinations {
        let Some(allow_from) = destination.allow_from else {
            continue;
        };
        let targets = workloads
            .iter()
            .filter(|workload| workload.app_id == destination.app_id);
        for target in targets {
            for source in allow_from {
                let (namespace, name) = source
                    .split_once('/')
                    .unwrap_or((destination.namespace, source.as_str()));
                plan.granted.extend(
                    workloads
                        .iter()
                        .filter(|workload| workload.namespace == namespace && workload.name == name)
                        .map(|workload| (workload.address, target.address)),
                );
            }
        }
    }
    plan
}

/// Every real destination the connect hooks hold to namespace isolation,
/// for `destination_map`:
///
/// - each backend in the service view: a local container address and port,
///   or a remote node address and published host port;
/// - each backend the cluster catalogue lists on any node, this one
///   included, at its node address and published host port (the DNAT
///   target another node's workload would dial);
/// - every port of each local workload address.
///
/// An address two owners claim at once (a stale catalogue meeting a reused
/// port) becomes contested and denies every namespaced caller.
pub fn destination_entries(
    services: &[ServiceEntry],
    catalog: &crate::onion::catalog::EndpointCatalog,
    workloads: &[LocalWorkload],
) -> std::collections::BTreeMap<DestinationKey, DestinationValue> {
    let mut entries = std::collections::BTreeMap::new();
    let mut claim = |key: DestinationKey, value: DestinationValue| {
        entries
            .entry(key)
            .and_modify(|current: &mut DestinationValue| {
                if *current != value {
                    *current = DestinationValue {
                        app_id: 0,
                        namespace_id: NAMESPACE_CONTESTED,
                    };
                }
            })
            .or_insert(value);
    };
    for service in services {
        let owner = DestinationValue {
            app_id: service.app_id,
            namespace_id: service.namespace_id,
        };
        for backend in &service.backends {
            claim(
                DestinationKey::new(backend.node_ip, backend.host_port),
                owner,
            );
        }
    }
    for (qualified, service) in &catalog.services {
        let Some(id) = crate::onion::service_id::ServiceId::parse(qualified) else {
            continue;
        };
        let owner = DestinationValue {
            app_id: u32::from(service.vip.0),
            namespace_id: crate::onion::vip::name_to_id(&id.namespace),
        };
        for backend in &service.backends {
            claim(
                DestinationKey::new(backend.node_ip, backend.host_port),
                owner,
            );
        }
    }
    for workload in workloads {
        claim(
            DestinationKey::any_port(workload.address),
            DestinationValue {
                app_id: workload.app_id,
                namespace_id: crate::onion::vip::name_to_id(&workload.namespace),
            },
        );
    }
    entries
}

/// Resolve cgroup-to-namespace mappings for all running instances.
pub fn resolve_cgroup_namespace_entries(
    cgroup_ids: &HashMap<(String, String), Vec<u64>>,
) -> Vec<CgroupNamespaceEntry> {
    let mut entries = Vec::new();
    for ((namespace, _app), cgroups) in cgroup_ids {
        for &cgroup_id in cgroups {
            entries.push(CgroupNamespaceEntry {
                cgroup_id,
                namespace_id: crate::onion::vip::name_to_id(namespace),
            });
        }
    }
    entries
}

/// Convert resolved rules to BPF map key/value pairs.
pub fn rules_to_bpf_entries(rules: &[ResolvedFirewallRule]) -> Vec<(FirewallKey, FirewallValue)> {
    rules
        .iter()
        .map(|r| {
            (
                FirewallKey {
                    src_cgroup_id: r.src_cgroup_id,
                    dst_app_id: r.dst_app_id,
                    _pad: 0,
                },
                FirewallValue { action: r.action },
            )
        })
        .collect()
}

/// Keys present in a previous reconcile but no longer desired — the entries
/// the agent must delete from a BPF map to converge it to `desired`. Used for
/// both `firewall_map` and `cgroup_namespace_map`, which the agent rebuilds
/// from scratch on every service-map mutation (NET5).
pub fn keys_to_delete<K: Eq + Hash + Copy>(previous: &HashSet<K>, desired: &HashSet<K>) -> Vec<K> {
    previous.difference(desired).copied().collect()
}

/// Errors from writing the eBPF firewall maps.
#[derive(Debug, thiserror::Error)]
pub enum FirewallMapError {
    #[error("firewall eBPF maps require Linux with --features ebpf")]
    Unsupported,

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    #[error("firewall map operation failed: {0}")]
    MapError(#[from] aya::maps::MapError),

    #[error("firewall map {map_name:?} not found in the loaded program")]
    MapNotFound { map_name: &'static str },
}

/// eBPF firewall map writers. With the `ebpf` feature these write the
/// `firewall_map` (per (src_cgroup, dst_app) allow entries) and the
/// `cgroup_namespace_map` (cgroup → namespace, which makes the connect
/// hook enforce cross-namespace isolation at all); without it they are
/// absent. The connect hook is already implemented in `ebpf/onion_connect.bpf.c`.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
mod maps {
    use super::{
        DestinationKey, DestinationValue, FirewallKey, FirewallMapError, FirewallValue,
        LiveFirewallState,
    };
    use aya::maps::HashMap;

    fn deletion_result(result: Result<(), aya::maps::MapError>) -> Result<(), FirewallMapError> {
        match result {
            Ok(()) | Err(aya::maps::MapError::KeyNotFound) => Ok(()),
            Err(aya::maps::MapError::SyscallError(error))
                if error.io_error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Allow a single `(src_cgroup, dst_app)` cross-namespace connection.
    pub fn write_firewall_entry(
        bpf: &mut aya::Ebpf,
        key: FirewallKey,
        value: FirewallValue,
    ) -> Result<(), FirewallMapError> {
        let mut map: HashMap<_, FirewallKey, FirewallValue> = HashMap::try_from(
            bpf.map_mut("firewall_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "firewall_map",
                })?,
        )?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Remove an allow entry, accepting only confirmed deletion or absence.
    pub fn delete_firewall_entry(
        bpf: &mut aya::Ebpf,
        key: FirewallKey,
    ) -> Result<(), FirewallMapError> {
        let mut map: HashMap<_, FirewallKey, FirewallValue> = HashMap::try_from(
            bpf.map_mut("firewall_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "firewall_map",
                })?,
        )?;
        deletion_result(map.remove(&key))
    }

    /// Record which namespace a cgroup belongs to. Once this is set the
    /// connect hook compares the source's namespace against the destination
    /// service's and denies a cross-namespace connect unless `firewall_map`
    /// allows it — so populating this map is what turns isolation *on*.
    pub fn write_cgroup_namespace_entry(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
        namespace_id: u32,
    ) -> Result<(), FirewallMapError> {
        let mut map: HashMap<_, u64, u32> = HashMap::try_from(
            bpf.map_mut("cgroup_namespace_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "cgroup_namespace_map",
                })?,
        )?;
        map.insert(cgroup_id, namespace_id, 0)?;
        Ok(())
    }

    /// Forget a cgroup's namespace (on instance stop), so a reused cgroup
    /// inode never inherits a departed workload's isolation identity.
    pub fn delete_cgroup_namespace_entry(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
    ) -> Result<(), FirewallMapError> {
        let mut map: HashMap<_, u64, u32> = HashMap::try_from(
            bpf.map_mut("cgroup_namespace_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "cgroup_namespace_map",
                })?,
        )?;
        deletion_result(map.remove(&cgroup_id))
    }

    /// List every cgroup id currently recorded in `cgroup_namespace_map`
    /// — the kernel truth the periodic sweep compares against the ids the
    /// reconcile pass last wrote, so entries for departed cgroups get
    /// deleted even after a Bun restart lost the in-memory bookkeeping.
    pub fn list_cgroup_namespace_keys(
        bpf: &mut aya::Ebpf,
    ) -> Result<std::collections::HashSet<u64>, FirewallMapError> {
        let map: HashMap<_, u64, u32> = HashMap::try_from(
            bpf.map_mut("cgroup_namespace_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "cgroup_namespace_map",
                })?,
        )?;
        map.keys().collect::<Result<_, _>>().map_err(Into::into)
    }

    /// Read all firewall keys without treating a failed observation as absence.
    pub fn list_firewall_keys(
        bpf: &mut aya::Ebpf,
    ) -> Result<std::collections::HashSet<FirewallKey>, FirewallMapError> {
        let map: HashMap<_, FirewallKey, FirewallValue> = HashMap::try_from(
            bpf.map_mut("firewall_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "firewall_map",
                })?,
        )?;
        map.keys().collect::<Result<_, _>>().map_err(Into::into)
    }

    /// Retire one original source's allow rules before removing its namespace.
    /// A failure retains the caller's ownership obligation for a later retry.
    pub fn delete_cgroup_firewall_state(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
    ) -> Result<(), FirewallMapError> {
        for key in list_firewall_keys(bpf)? {
            if key.src_cgroup_id == cgroup_id {
                delete_firewall_entry(bpf, key)?;
            }
        }
        delete_cgroup_namespace_entry(bpf, cgroup_id)
    }

    /// Remove every grant to an originally owned destination before its VIP
    /// becomes reusable. Other destinations and source identities are retained.
    pub fn delete_destination_firewall_state(
        bpf: &mut aya::Ebpf,
        destination_app_id: u32,
    ) -> Result<(), FirewallMapError> {
        for key in list_firewall_keys(bpf)? {
            if key.dst_app_id == destination_app_id {
                delete_firewall_entry(bpf, key)?;
            }
        }
        Ok(())
    }

    /// Reconcile namespace and firewall entries, retaining keys until removal.
    pub fn reconcile_firewall_maps(
        bpf: &mut aya::Ebpf,
        namespace_entries: &[super::CgroupNamespaceEntry],
        firewall_entries: &[(FirewallKey, FirewallValue)],
        namespace_keys: &mut std::collections::HashSet<u64>,
        firewall_keys: &mut std::collections::HashSet<FirewallKey>,
    ) -> Result<(), FirewallMapError> {
        let desired_namespaces = namespace_entries
            .iter()
            .map(|entry| entry.cgroup_id)
            .collect();
        let desired_firewall = firewall_entries.iter().map(|(key, _)| *key).collect();
        for entry in namespace_entries {
            // A failed syscall is not permission to forget attempted ownership.
            namespace_keys.insert(entry.cgroup_id);
            write_cgroup_namespace_entry(bpf, entry.cgroup_id, entry.namespace_id)?;
        }
        for (key, value) in firewall_entries {
            firewall_keys.insert(*key);
            write_firewall_entry(bpf, *key, *value)?;
        }
        // Keep namespace enforcement until obsolete allow rules are removed.
        for key in super::keys_to_delete(firewall_keys, &desired_firewall) {
            delete_firewall_entry(bpf, key)?;
            firewall_keys.remove(&key);
        }
        for key in super::keys_to_delete(namespace_keys, &desired_namespaces) {
            delete_cgroup_namespace_entry(bpf, key)?;
            namespace_keys.remove(&key);
        }
        Ok(())
    }

    fn destination_map(
        bpf: &mut aya::Ebpf,
    ) -> Result<HashMap<&mut aya::maps::MapData, DestinationKey, DestinationValue>, FirewallMapError>
    {
        Ok(HashMap::try_from(bpf.map_mut("destination_map").ok_or(
            FirewallMapError::MapNotFound {
                map_name: "destination_map",
            },
        )?)?)
    }

    /// Record who owns one real destination.
    pub fn write_destination_entry(
        bpf: &mut aya::Ebpf,
        key: DestinationKey,
        value: DestinationValue,
    ) -> Result<(), FirewallMapError> {
        destination_map(bpf)?.insert(key, value, 0)?;
        Ok(())
    }

    /// Forget one real destination; an absent key is already forgotten.
    pub fn delete_destination_entry(
        bpf: &mut aya::Ebpf,
        key: DestinationKey,
    ) -> Result<(), FirewallMapError> {
        deletion_result(destination_map(bpf)?.remove(&key))
    }

    /// Read every real destination the kernel currently holds to isolation.
    pub fn list_destination_entries(
        bpf: &mut aya::Ebpf,
    ) -> Result<std::collections::BTreeMap<DestinationKey, DestinationValue>, FirewallMapError>
    {
        let map = destination_map(bpf)?;
        map.iter().collect::<Result<_, _>>().map_err(Into::into)
    }

    /// Converge `destination_map` to `desired`. New and changed owners are
    /// written before departed ones are deleted, and `keys` remembers every
    /// key this node attempted, so a failure never forgets one it may own.
    pub fn reconcile_destination_map(
        bpf: &mut aya::Ebpf,
        desired: &std::collections::BTreeMap<DestinationKey, DestinationValue>,
        keys: &mut std::collections::HashSet<DestinationKey>,
    ) -> Result<(), FirewallMapError> {
        let mut map = destination_map(bpf)?;
        for (key, value) in desired {
            keys.insert(*key);
            map.insert(*key, *value, 0)?;
        }
        let wanted: std::collections::HashSet<DestinationKey> = desired.keys().copied().collect();
        for key in super::keys_to_delete(keys, &wanted) {
            deletion_result(map.remove(&key))?;
            keys.remove(&key);
        }
        Ok(())
    }

    /// Read the exact namespace and allow values the live connect hook would
    /// consult for one source/destination pair.
    pub fn read_firewall_state(
        bpf: &mut aya::Ebpf,
        source_cgroup_id: u64,
        destination_app_id: u32,
    ) -> Result<LiveFirewallState, FirewallMapError> {
        let source_namespace_id = {
            let namespace_map: HashMap<_, u64, u32> = HashMap::try_from(
                bpf.map_mut("cgroup_namespace_map")
                    .ok_or(FirewallMapError::MapNotFound {
                        map_name: "cgroup_namespace_map",
                    })?,
            )?;
            match namespace_map.get(&source_cgroup_id, 0) {
                Ok(namespace_id) => Some(namespace_id),
                Err(aya::maps::MapError::KeyNotFound) => None,
                Err(error) => return Err(error.into()),
            }
        };

        let firewall_map: HashMap<_, FirewallKey, FirewallValue> = HashMap::try_from(
            bpf.map_mut("firewall_map")
                .ok_or(FirewallMapError::MapNotFound {
                    map_name: "firewall_map",
                })?,
        )?;
        let key = FirewallKey {
            src_cgroup_id: source_cgroup_id,
            dst_app_id: destination_app_id,
            _pad: 0,
        };
        let action = match firewall_map.get(&key, 0) {
            Ok(value) => Some(value.action),
            Err(aya::maps::MapError::KeyNotFound) => None,
            Err(error) => return Err(error.into()),
        };

        Ok(LiveFirewallState {
            source_namespace_id,
            action,
        })
    }
}

#[cfg(all(feature = "ebpf", target_os = "linux"))]
pub use maps::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onion::types::{BackendInstance, ServiceEntry};
    use crate::onion::vip::VirtualIP;
    use std::net::Ipv4Addr;

    /// Build a `(namespace, app)`-keyed cgroup-id map entry.
    fn cg(namespace: &str, app: &str, ids: Vec<u64>) -> ((String, String), Vec<u64>) {
        ((namespace.to_string(), app.to_string()), ids)
    }

    fn make_service(
        name: &str,
        namespace: &str,
        app_id: u32,
        ns_id: u32,
        allow_from: Option<Vec<String>>,
    ) -> ServiceEntry {
        ServiceEntry {
            app_name: name.to_string(),
            namespace: namespace.to_string(),
            namespace_id: ns_id,
            app_id,
            vip: VirtualIP(Ipv4Addr::new(127, 128, 0, app_id as u8)),
            port: 8080,
            backends: vec![BackendInstance {
                instance_id: format!("{name}-0"),
                node_ip: Ipv4Addr::new(10, 0, 1, 1),
                host_port: 30000,
                healthy: true,
                local: false,
            }],
            firewall_allow_from: allow_from,
        }
    }

    fn catalog_with(
        namespace: &str,
        name: &str,
        vip: Ipv4Addr,
        backends: &[(&str, Ipv4Addr, u16)],
    ) -> crate::onion::catalog::EndpointCatalog {
        let mut catalog = crate::onion::catalog::EndpointCatalog::new();
        catalog.services.insert(
            crate::onion::service_id::ServiceId::new(namespace, name).qualified(),
            crate::onion::catalog::CatalogService {
                vip: VirtualIP(vip),
                port: 8080,
                backends: backends
                    .iter()
                    .map(|(node, ip, port)| crate::onion::catalog::CatalogBackend {
                        execution: None,
                        node_id: (*node).into(),
                        node_ip: *ip,
                        host_port: *port,
                        healthy: true,
                    })
                    .collect(),
            },
        );
        catalog
    }

    #[test]
    fn destinations_cover_backend_addresses_and_published_host_ports() {
        let mut local = make_service("db", "backend", 7, 70, None);
        local.backends[0].node_ip = Ipv4Addr::new(10, 88, 0, 5);
        local.backends[0].host_port = 5432;
        let catalog = catalog_with(
            "backend",
            "db",
            Ipv4Addr::new(127, 128, 0, 7),
            &[
                ("this-node", Ipv4Addr::new(192, 168, 0, 1), 31000),
                ("other-node", Ipv4Addr::new(192, 168, 0, 2), 31001),
            ],
        );
        let entries = destination_entries(&[local], &catalog, &[]);
        let owner = |ip, port| entries.get(&DestinationKey::new(ip, port)).copied();
        // The local container address and port.
        assert_eq!(
            owner(Ipv4Addr::new(10, 88, 0, 5), 5432),
            Some(DestinationValue {
                app_id: 7,
                namespace_id: 70
            })
        );
        // Every node's published host port, this node's included.
        let catalogued = DestinationValue {
            app_id: u32::from(Ipv4Addr::new(127, 128, 0, 7)),
            namespace_id: crate::onion::vip::name_to_id("backend"),
        };
        assert_eq!(
            owner(Ipv4Addr::new(192, 168, 0, 1), 31000),
            Some(catalogued)
        );
        assert_eq!(
            owner(Ipv4Addr::new(192, 168, 0, 2), 31001),
            Some(catalogued)
        );
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn container_addresses_are_owned_on_every_port() {
        let job = workload_app_id("batch", "crawler");
        let entries = destination_entries(
            &[],
            &crate::onion::catalog::EndpointCatalog::new(),
            &[LocalWorkload {
                address: Ipv4Addr::new(10, 88, 0, 9),
                namespace: "batch".into(),
                name: "crawler".into(),
                app_id: job,
            }],
        );
        assert_eq!(
            entries.get(&DestinationKey::any_port(Ipv4Addr::new(10, 88, 0, 9))),
            Some(&DestinationValue {
                app_id: job,
                namespace_id: crate::onion::vip::name_to_id("batch")
            })
        );
    }

    #[test]
    fn the_forward_path_opens_only_what_allow_from_grants() {
        let workload = |last: u8, namespace: &str, name: &str| LocalWorkload {
            address: Ipv4Addr::new(10, 88, 0, last),
            namespace: namespace.into(),
            name: name.into(),
            app_id: workload_app_id(namespace, name),
        };
        let workloads = [
            workload(2, "batch", "crawler"),
            workload(3, "frontend", "scraper"),
            workload(4, "frontend", "other"),
        ];
        let allow = vec!["frontend/scraper".to_string()];
        let plan = isolation_plan(
            &workloads,
            &[IsolatedDestination {
                namespace: "batch",
                name: "crawler",
                app_id: workload_app_id("batch", "crawler"),
                allow_from: Some(&allow),
            }],
        );
        assert_eq!(
            plan.granted,
            std::collections::BTreeSet::from([(
                Ipv4Addr::new(10, 88, 0, 3),
                Ipv4Addr::new(10, 88, 0, 2)
            )])
        );
        assert_eq!(
            plan.namespaces[&crate::onion::vip::name_to_id("frontend")].len(),
            2
        );
    }

    #[test]
    fn an_address_two_owners_claim_denies_every_namespace() {
        let first = make_service("db", "backend", 7, 70, None);
        let second = make_service("cache", "other", 8, 80, None);
        // Both name 10.0.1.1:30000, the test service's backend address.
        let entries = destination_entries(
            &[first, second],
            &crate::onion::catalog::EndpointCatalog::new(),
            &[],
        );
        assert_eq!(
            entries.get(&DestinationKey::new(Ipv4Addr::new(10, 0, 1, 1), 30000)),
            Some(&DestinationValue {
                app_id: 0,
                namespace_id: NAMESPACE_CONTESTED
            })
        );
    }

    #[test]
    fn job_identities_never_collide_with_service_identities() {
        let job = workload_app_id("default", "nightly");
        assert!(
            job >= 0x8000_0000,
            "service app ids are VIPs below 0x8000_0000"
        );
        assert_eq!(job, workload_app_id("default", "nightly"));
        assert_ne!(job, workload_app_id("other", "nightly"));
    }

    #[test]
    fn job_allow_from_grants_named_sources_another_namespace() {
        let allow = vec!["frontend/scraper".to_string()];
        let destination = IsolatedDestination {
            namespace: "batch",
            name: "crawler",
            app_id: workload_app_id("batch", "crawler"),
            allow_from: Some(&allow),
        };
        let cgroups = [
            cg("frontend", "scraper", vec![41]),
            cg("frontend", "other", vec![42]),
        ]
        .into();
        let rules = resolve_destination_rules(&[destination], &cgroups);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].src_cgroup_id, 41);
        assert_eq!(rules[0].dst_app_id, destination.app_id);
    }

    #[test]
    fn outbound_only_workloads_receive_namespace_identity_and_allowed_routes() {
        let services = vec![make_service(
            "db",
            "backend",
            2,
            200,
            Some(vec!["frontend/worker".into()]),
        )];
        let cgroups = [cg("frontend", "worker", vec![1001])].into();
        let entries = resolve_cgroup_namespace_entries(&cgroups);
        assert_eq!(entries.len(), 1, "outbound-only source has no namespace");
        assert_eq!(entries[0].cgroup_id, 1001);
        assert_eq!(
            entries[0].namespace_id,
            crate::onion::vip::name_to_id("frontend")
        );
        let rules = resolve_firewall_rules(&services, &cgroups);
        assert_eq!(
            rules.len(),
            1,
            "explicitly allowed source needs no service port"
        );
        assert_eq!(rules[0].src_cgroup_id, 1001);
        assert_eq!(rules[0].dst_app_id, 2);
    }

    #[test]
    fn default_allows_same_namespace() {
        let services = vec![
            make_service("api", "default", 1, 100, None),
            make_service("redis", "default", 2, 100, None),
        ];
        let cgroups: HashMap<(String, String), Vec<u64>> = [
            cg("default", "api", vec![1001]),
            cg("default", "redis", vec![1002]),
        ]
        .into();

        let rules = resolve_firewall_rules(&services, &cgroups);
        // api→redis and redis→api should both be allowed
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|r| r.action == FIREWALL_ALLOW));
    }

    #[test]
    fn cross_namespace_denied_by_default() {
        let services = vec![
            make_service("api", "frontend", 1, 100, None),
            make_service("db", "backend", 2, 200, None),
        ];
        let cgroups: HashMap<(String, String), Vec<u64>> = [
            cg("frontend", "api", vec![1001]),
            cg("backend", "db", vec![1002]),
        ]
        .into();

        let rules = resolve_firewall_rules(&services, &cgroups);
        // No rules: different namespaces with no explicit allow_from
        assert!(rules.is_empty());
    }

    #[test]
    fn explicit_allow_from_permits_cross_namespace() {
        let services = vec![
            make_service("api", "frontend", 1, 100, None),
            make_service(
                "db",
                "backend",
                2,
                200,
                Some(vec!["frontend/api".to_string()]),
            ),
        ];
        let cgroups: HashMap<(String, String), Vec<u64>> = [
            cg("frontend", "api", vec![1001]),
            cg("backend", "db", vec![1002]),
        ]
        .into();

        let rules = resolve_firewall_rules(&services, &cgroups);
        // db allows api from frontend namespace
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].src_cgroup_id, 1001);
        assert_eq!(rules[0].dst_app_id, 2);
        assert_eq!(rules[0].action, FIREWALL_ALLOW);
    }

    #[test]
    fn cgroup_namespace_entries_resolve_correctly() {
        let cgroups: HashMap<(String, String), Vec<u64>> = [
            cg("default", "api", vec![1001, 1002]),
            cg("default", "redis", vec![2001]),
        ]
        .into();

        let entries = resolve_cgroup_namespace_entries(&cgroups);
        assert_eq!(entries.len(), 3);
        assert!(
            entries
                .iter()
                .all(|e| e.namespace_id == crate::onion::vip::name_to_id("default"))
        );
    }

    #[test]
    fn same_named_apps_in_different_namespaces_do_not_collide() {
        // H9: `web` runs in both `team-a` and `team-b`. Keying cgroup ids by
        // bare app name conflated them — an `allow_from` rule for one, and the
        // namespace mapping, leaked to the other. With `(namespace, app)` keys
        // each `web` only ever sees its own cgroup.
        let services = vec![
            make_service("web", "team-a", 1, 100, None),
            make_service("client", "team-a", 2, 100, Some(vec!["web".to_string()])),
            make_service("web", "team-b", 3, 200, None),
        ];
        let cgroups: HashMap<(String, String), Vec<u64>> = [
            cg("team-a", "web", vec![1001]),
            cg("team-a", "client", vec![1002]),
            cg("team-b", "web", vec![9001]),
        ]
        .into();

        // client (team-a) allows web: only team-a's web cgroup 1001, never
        // team-b's web cgroup 9001.
        let rules = resolve_firewall_rules(&services, &cgroups);
        let client_id = 2;
        let sources: Vec<u64> = rules
            .iter()
            .filter(|r| r.dst_app_id == client_id)
            .map(|r| r.src_cgroup_id)
            .collect();
        assert_eq!(sources, vec![1001], "team-b's web must not be allowed");

        // Namespace mapping keeps the same-named source in its own namespace.
        let entries = resolve_cgroup_namespace_entries(&cgroups);
        let team_b_web = entries.iter().find(|e| e.cgroup_id == 9001).unwrap();
        assert_eq!(
            team_b_web.namespace_id,
            crate::onion::vip::name_to_id("team-b")
        );
    }

    #[test]
    fn keys_to_delete_returns_only_departed_keys() {
        // A reconcile where cgroup 2001 went away and 3001 arrived: only the
        // departed key is deleted; the surviving one is left in place and the
        // new one is a write, not a delete.
        let previous: HashSet<u64> = [1001, 2001].into();
        let desired: HashSet<u64> = [1001, 3001].into();
        let mut deletes = keys_to_delete(&previous, &desired);
        deletes.sort();
        assert_eq!(deletes, vec![2001]);
    }

    #[test]
    fn keys_to_delete_is_empty_when_nothing_departed() {
        let previous: HashSet<u64> = [1001].into();
        let desired: HashSet<u64> = [1001, 2001].into();
        assert!(keys_to_delete(&previous, &desired).is_empty());
    }
}
