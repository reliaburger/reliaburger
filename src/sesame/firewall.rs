//! eBPF firewall map wiring.
//!
//! Populates the `firewall_map` and `cgroup_namespace_map` BPF maps
//! from app configuration `allow_from` rules. The eBPF connect hook
//! (already implemented in Onion) checks these maps on every `connect()`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::onion::types::{FirewallKey, FirewallValue, ServiceEntry};

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

/// Live kernel values used by `relish trace` to mirror the connect hook's
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
    let mut rules = Vec::new();

    for service in services {
        let dst_app_id = service.app_id;

        match &service.firewall_allow_from {
            None => {
                // Default: allow all apps in the same namespace
                for other in services {
                    if other.namespace == service.namespace
                        && other.app_name != service.app_name
                        && let Some(cgroups) =
                            cgroup_ids.get(&(other.namespace.clone(), other.app_name.clone()))
                    {
                        for &cg in cgroups {
                            rules.push(ResolvedFirewallRule {
                                src_cgroup_id: cg,
                                dst_app_id,
                                action: FIREWALL_ALLOW,
                            });
                        }
                    }
                }
            }
            Some(allow_list) => {
                // Explicit allow list
                for allowed_name in allow_list {
                    // Support "namespace/app" or just "app" (same namespace)
                    let (target_ns, target_app) =
                        if let Some((ns, app)) = allowed_name.split_once('/') {
                            (ns, app)
                        } else {
                            (service.namespace.as_str(), allowed_name.as_str())
                        };

                    // Find the allowed app's cgroup IDs
                    let matching_app = services
                        .iter()
                        .find(|s| s.app_name == target_app && s.namespace == target_ns);

                    if matching_app.is_some()
                        && let Some(cgroups) =
                            cgroup_ids.get(&(target_ns.to_string(), target_app.to_string()))
                    {
                        for &cg in cgroups {
                            rules.push(ResolvedFirewallRule {
                                src_cgroup_id: cg,
                                dst_app_id,
                                action: FIREWALL_ALLOW,
                            });
                        }
                    }
                }
            }
        }
    }

    rules
}

/// Resolve cgroup-to-namespace mappings for all running instances.
pub fn resolve_cgroup_namespace_entries(
    services: &[ServiceEntry],
    cgroup_ids: &HashMap<(String, String), Vec<u64>>,
) -> Vec<CgroupNamespaceEntry> {
    let mut entries = Vec::new();
    for service in services {
        if let Some(cgroups) =
            cgroup_ids.get(&(service.namespace.clone(), service.app_name.clone()))
        {
            for &cg in cgroups {
                entries.push(CgroupNamespaceEntry {
                    cgroup_id: cg,
                    namespace_id: service.namespace_id,
                });
            }
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
    use super::{FirewallKey, FirewallMapError, FirewallValue, LiveFirewallState};
    use aya::maps::HashMap;

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

    /// Remove a previously written firewall allow entry.
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
        let _ = map.remove(&key);
        Ok(())
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
        let _ = map.remove(&cgroup_id);
        Ok(())
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
        Ok(map.keys().filter_map(|k| k.ok()).collect())
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
            }],
            firewall_allow_from: allow_from,
        }
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
        let services = vec![
            make_service("api", "default", 1, 100, None),
            make_service("redis", "default", 2, 100, None),
        ];
        let cgroups: HashMap<(String, String), Vec<u64>> = [
            cg("default", "api", vec![1001, 1002]),
            cg("default", "redis", vec![2001]),
        ]
        .into();

        let entries = resolve_cgroup_namespace_entries(&services, &cgroups);
        assert_eq!(entries.len(), 3);
        // All should map to namespace_id 100
        assert!(entries.iter().all(|e| e.namespace_id == 100));
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

        // Namespace mapping: team-b's web maps to namespace 200, not 100.
        let entries = resolve_cgroup_namespace_entries(&services, &cgroups);
        let team_b_web = entries.iter().find(|e| e.cgroup_id == 9001).unwrap();
        assert_eq!(team_b_web.namespace_id, 200);
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
