//! Network faults on the caller's node.
//!
//! A network fault changes what happens when something *calls* its target, so
//! it has to be installed wherever the callers run. Callers come and go: a
//! replica restarts, a deploy rolls, the scheduler moves a source app to this
//! node. So instead of writing kernel state once when a fault is injected,
//! the agent recomputes the state every active fault *should* produce from
//! the instances running right now, and converges on it. This module is the
//! pure half of that: given the active faults and the local callers, what
//! should the eBPF `fault_connect_map` hold?

use std::collections::BTreeMap;

use super::types::{FaultRule, FaultType};

/// A workload instance on this node that may call a faulted service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCaller {
    /// Node-local instance id, e.g. `default/frontend-0`.
    pub instance_id: String,
    /// The instance's app.
    pub app: String,
    /// The instance's namespace.
    pub namespace: String,
    /// The cgroup id the eBPF connect hook sees for this instance, when the
    /// runtime can prove one.
    pub cgroup_id: Option<u64>,
}

/// Whether `rule` acts on calls made by an instance of `app` in `namespace`.
///
/// A fault limited to a source app acts only on that app in the fault's own
/// namespace; a fault on every caller acts on everything that calls the
/// target, whatever namespace it runs in.
pub fn applies_to_caller(rule: &FaultRule, app: &str, namespace: &str) -> bool {
    match rule.fault_type.source_app() {
        Some(source) => source == app && rule.matches_namespace(namespace),
        None => true,
    }
}

/// What the connect hook does with a matching connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFaultAction {
    /// Refuse this percentage of connections with EPERM.
    Drop {
        /// 0-100.
        probability: u8,
    },
    /// Refuse every connection with EPERM.
    Partition,
}

/// One desired `fault_connect_map` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectFaultEntry {
    /// What to do with the connection.
    pub action: ConnectFaultAction,
    /// CLOCK_MONOTONIC expiry, checked by the kernel too.
    pub expires_ns: u64,
}

/// A `fault_connect_map` key: virtual IP and port in network byte order, and
/// the source cgroup id (0 matches every caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectFaultKey {
    /// Service virtual IP, network byte order.
    pub virtual_ip: u32,
    /// Service port, network byte order.
    pub port: u16,
    /// Source cgroup id, or 0 for every caller.
    pub source_cgroup_id: u64,
}

impl ConnectFaultEntry {
    /// Which of two faults on the same key the kernel should enforce: a
    /// partition beats a drop, a likelier drop beats a gentler one, and the
    /// longer-lived fault wins a tie, so clearing either leaves the other.
    fn outranks(&self, other: &Self) -> bool {
        let rank = |entry: &Self| match entry.action {
            ConnectFaultAction::Partition => (101u8, entry.expires_ns),
            ConnectFaultAction::Drop { probability } => (probability, entry.expires_ns),
        };
        rank(self) > rank(other)
    }
}

/// The `fault_connect_map` contents that the active faults ask for on this
/// node.
///
/// `resolve` maps a fault to its target's (virtual IP, port) in network byte
/// order, or `None` when this node doesn't know the service yet. A fault on
/// every caller becomes one wildcard key; a fault from one source becomes one
/// key per local instance of that source whose cgroup is known. Where two
/// faults want the same key, the stronger one wins (see `outranks`).
pub fn desired_connect_faults<'a>(
    rules: impl IntoIterator<Item = &'a FaultRule>,
    resolve: impl Fn(&FaultRule) -> Option<(u32, u16)>,
    callers: &[LocalCaller],
) -> BTreeMap<ConnectFaultKey, ConnectFaultEntry> {
    let mut desired: BTreeMap<ConnectFaultKey, ConnectFaultEntry> = BTreeMap::new();
    for rule in rules {
        let action = match &rule.fault_type {
            FaultType::Drop { probability } => ConnectFaultAction::Drop {
                probability: *probability,
            },
            FaultType::Partition { .. } => ConnectFaultAction::Partition,
            _ => continue,
        };
        let Some((virtual_ip, port)) = resolve(rule) else {
            continue;
        };
        let entry = ConnectFaultEntry {
            action,
            expires_ns: rule.expires_at_ns,
        };
        let cgroups: Vec<u64> = match rule.fault_type.source_app() {
            None => vec![0],
            Some(_) => callers
                .iter()
                .filter(|caller| applies_to_caller(rule, &caller.app, &caller.namespace))
                .filter_map(|caller| caller.cgroup_id)
                .collect(),
        };
        for source_cgroup_id in cgroups {
            let key = ConnectFaultKey {
                virtual_ip,
                port,
                source_cgroup_id,
            };
            match desired.get(&key) {
                Some(existing) if !entry.outranks(existing) => {}
                _ => {
                    desired.insert(key, entry);
                }
            }
        }
    }
    desired
}

/// The difference between what is installed and what should be.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectFaultChanges {
    /// Keys to write, new or with a changed value.
    pub write: Vec<(ConnectFaultKey, ConnectFaultEntry)>,
    /// Keys that no fault wants any more.
    pub delete: Vec<ConnectFaultKey>,
}

/// Compare the installed map with the desired one.
pub fn connect_fault_changes(
    installed: &BTreeMap<ConnectFaultKey, ConnectFaultEntry>,
    desired: &BTreeMap<ConnectFaultKey, ConnectFaultEntry>,
) -> ConnectFaultChanges {
    ConnectFaultChanges {
        write: desired
            .iter()
            .filter(|(key, entry)| installed.get(key) != Some(entry))
            .map(|(key, entry)| (*key, *entry))
            .collect(),
        delete: installed
            .keys()
            .filter(|key| !desired.contains_key(key))
            .copied()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::smoker::types::FaultId;

    fn rule(id: u64, fault_type: FaultType) -> FaultRule {
        let mut rule = FaultRule::new(
            FaultId(id),
            fault_type,
            "redis".to_string(),
            Duration::from_secs(60),
            "test".to_string(),
        );
        rule.namespace = Some("default".to_string());
        rule
    }

    fn caller(id: &str, app: &str, namespace: &str, cgroup: Option<u64>) -> LocalCaller {
        LocalCaller {
            instance_id: id.to_string(),
            app: app.to_string(),
            namespace: namespace.to_string(),
            cgroup_id: cgroup,
        }
    }

    fn resolve(_rule: &FaultRule) -> Option<(u32, u16)> {
        Some((7, 99))
    }

    fn key(cgroup: u64) -> ConnectFaultKey {
        ConnectFaultKey {
            virtual_ip: 7,
            port: 99,
            source_cgroup_id: cgroup,
        }
    }

    #[test]
    fn a_fault_on_every_caller_is_one_wildcard_key() {
        let drop = rule(1, FaultType::Drop { probability: 30 });
        let desired = desired_connect_faults([&drop], resolve, &[]);
        assert_eq!(desired.keys().copied().collect::<Vec<_>>(), vec![key(0)]);
    }

    #[test]
    fn a_partition_from_one_app_keys_each_local_instance_in_its_namespace() {
        let partition = rule(
            1,
            FaultType::Partition {
                source_app: Some("frontend".to_string()),
            },
        );
        let callers = [
            caller("default/frontend-0", "frontend", "default", Some(11)),
            caller("default/frontend-1", "frontend", "default", Some(12)),
            caller("team-b/frontend-0", "frontend", "team-b", Some(13)),
            caller("default/backend-0", "backend", "default", Some(14)),
            caller("default/frontend-2", "frontend", "default", None),
        ];
        let desired = desired_connect_faults([&partition], resolve, &callers);
        assert_eq!(
            desired.keys().copied().collect::<Vec<_>>(),
            vec![key(11), key(12)]
        );
    }

    #[test]
    fn a_new_source_instance_adds_its_key_and_a_gone_one_removes_it() {
        let partition = rule(
            1,
            FaultType::Partition {
                source_app: Some("frontend".to_string()),
            },
        );
        let before = desired_connect_faults(
            [&partition],
            resolve,
            &[caller(
                "default/frontend-0",
                "frontend",
                "default",
                Some(11),
            )],
        );
        let after = desired_connect_faults(
            [&partition],
            resolve,
            &[caller(
                "default/frontend-0",
                "frontend",
                "default",
                Some(21),
            )],
        );
        let changes = connect_fault_changes(&before, &after);
        assert_eq!(
            changes
                .write
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>(),
            vec![key(21)]
        );
        assert_eq!(changes.delete, vec![key(11)]);
        assert_eq!(connect_fault_changes(&after, &after), Default::default());
    }

    #[test]
    fn the_stronger_fault_holds_a_shared_key_and_the_other_survives_its_clear() {
        let gentle = rule(1, FaultType::Drop { probability: 10 });
        let harsh = rule(2, FaultType::Partition { source_app: None });
        let both = desired_connect_faults([&gentle, &harsh], resolve, &[]);
        assert_eq!(both[&key(0)].action, ConnectFaultAction::Partition);
        let reversed = desired_connect_faults([&harsh, &gentle], resolve, &[]);
        assert_eq!(both, reversed);

        // Clearing the partition rewrites the key with the drop, rather than
        // deleting the key both faults shared.
        let after_clear = desired_connect_faults([&gentle], resolve, &[]);
        let changes = connect_fault_changes(&both, &after_clear);
        assert!(changes.delete.is_empty());
        assert_eq!(
            changes.write[0].1.action,
            ConnectFaultAction::Drop { probability: 10 }
        );
    }

    #[test]
    fn faults_on_unknown_services_and_other_kinds_install_nothing() {
        let drop = rule(1, FaultType::Drop { probability: 10 });
        let dns = rule(2, FaultType::DnsNxdomain);
        assert!(desired_connect_faults([&drop], |_| None, &[]).is_empty());
        assert!(desired_connect_faults([&dns], resolve, &[]).is_empty());
    }

    #[test]
    fn a_fault_on_every_caller_applies_across_namespaces() {
        let everyone = rule(1, FaultType::DnsNxdomain);
        assert!(applies_to_caller(&everyone, "anything", "team-b"));
        let scoped = rule(
            2,
            FaultType::Delay {
                delay_ns: 1,
                jitter_ns: 0,
                source_app: Some("frontend".to_string()),
            },
        );
        assert!(applies_to_caller(&scoped, "frontend", "default"));
        assert!(!applies_to_caller(&scoped, "frontend", "team-b"));
        assert!(!applies_to_caller(&scoped, "backend", "default"));
    }
}
