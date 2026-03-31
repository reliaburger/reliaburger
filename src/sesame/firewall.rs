//! eBPF firewall map wiring.
//!
//! Populates the `firewall_map` and `cgroup_namespace_map` BPF maps
//! from app configuration `allow_from` rules. The eBPF connect hook
//! (already implemented in Onion) checks these maps on every `connect()`.

use std::collections::HashMap;

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
    cgroup_ids: &HashMap<String, Vec<u64>>,
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
                        && let Some(cgroups) = cgroup_ids.get(&other.app_name)
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
                        && let Some(cgroups) = cgroup_ids.get(target_app)
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
    cgroup_ids: &HashMap<String, Vec<u64>>,
) -> Vec<CgroupNamespaceEntry> {
    let mut entries = Vec::new();
    for service in services {
        if let Some(cgroups) = cgroup_ids.get(&service.app_name) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onion::types::{BackendInstance, ServiceEntry};
    use crate::onion::vip::VirtualIP;
    use std::net::Ipv4Addr;

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
        let cgroups: HashMap<String, Vec<u64>> = [
            ("api".to_string(), vec![1001]),
            ("redis".to_string(), vec![1002]),
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
        let cgroups: HashMap<String, Vec<u64>> = [
            ("api".to_string(), vec![1001]),
            ("db".to_string(), vec![1002]),
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
        let cgroups: HashMap<String, Vec<u64>> = [
            ("api".to_string(), vec![1001]),
            ("db".to_string(), vec![1002]),
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
        let cgroups: HashMap<String, Vec<u64>> = [
            ("api".to_string(), vec![1001, 1002]),
            ("redis".to_string(), vec![2001]),
        ]
        .into();

        let entries = resolve_cgroup_namespace_entries(&services, &cgroups);
        assert_eq!(entries.len(), 3);
        // All should map to namespace_id 100
        assert!(entries.iter().all(|e| e.namespace_id == 100));
    }
}
