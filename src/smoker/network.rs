//! Network faults on the caller's node.
//!
//! A network fault changes what happens when something *calls* its target, so
//! it has to be installed wherever the callers run. Callers come and go: a
//! replica restarts, a deploy rolls, the scheduler moves a source app to this
//! node. So instead of writing kernel state once when a fault is injected,
//! the agent recomputes the state every active fault *should* produce from
//! the instances running right now, and converges on it. This module is the
//! pure half of that: given the active faults and the local callers, what
//! should the eBPF `fault_connect_map` hold, whose open connections should a
//! newly landed fault cut, and which netem delays should each caller's
//! interface carry? It also runs the few host tools (`ss`, `tc`) the agent
//! needs inside a container's network namespace.

use std::collections::BTreeMap;
use std::net::SocketAddrV4;

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

/// Whether writing `entry` over `previous` is a fault *landing* on its key:
/// the key is new, or it changed what it does (a drop became a partition).
/// A refreshed expiry is not a landing.
pub fn lands(previous: Option<&ConnectFaultEntry>, entry: &ConnectFaultEntry) -> bool {
    previous.is_none_or(|previous| previous.action != entry.action)
}

/// A caller's established connections that a newly landed fault should cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionCut {
    /// The caller instance whose network namespace holds the connections.
    pub instance_id: String,
    /// Backend addresses (after the VIP rewrite) to cut connections to.
    pub backends: Vec<SocketAddrV4>,
}

/// Which callers' open connections the newly landed keys should cut.
///
/// The connect hook only sees *new* connections, so a pooled client (a Redis
/// or database pool, an HTTP keep-alive) would carry on as if nothing had
/// happened. Cutting its established sockets makes it reconnect, and the
/// reconnect meets the fault. A wildcard key cuts every local caller; a
/// source-scoped key cuts the instance with that cgroup. `backends` gives the
/// post-rewrite addresses behind a (virtual IP, port) pair: the sockets are
/// connected to those, never to the VIP.
pub fn connections_to_cut(
    landed: &[ConnectFaultKey],
    callers: &[LocalCaller],
    backends: impl Fn(u32, u16) -> Vec<SocketAddrV4>,
) -> Vec<ConnectionCut> {
    let mut cuts: BTreeMap<&str, Vec<SocketAddrV4>> = BTreeMap::new();
    for key in landed {
        let addresses = backends(key.virtual_ip, key.port);
        if addresses.is_empty() {
            continue;
        }
        for caller in callers {
            let affected =
                key.source_cgroup_id == 0 || caller.cgroup_id == Some(key.source_cgroup_id);
            if affected {
                cuts.entry(caller.instance_id.as_str())
                    .or_default()
                    .extend(addresses.iter().copied());
            }
        }
    }
    cuts.into_iter()
        .map(|(instance_id, mut backends)| {
            backends.sort_unstable();
            backends.dedup();
            ConnectionCut {
                instance_id: instance_id.to_string(),
                backends,
            }
        })
        .collect()
}

/// `ss` arguments that destroy every established TCP connection to one of
/// `backends` (`ss -K` needs a kernel built with `CONFIG_INET_DIAG_DESTROY`).
pub fn socket_destroy_args(backends: &[SocketAddrV4]) -> Vec<String> {
    let mut args: Vec<String> = ["-K", "-tn", "state", "established", "("]
        .into_iter()
        .map(str::to_string)
        .collect();
    for (index, backend) in backends.iter().enumerate() {
        if index > 0 {
            args.push("or".to_string());
        }
        args.push("dst".to_string());
        args.push(backend.to_string());
    }
    args.push(")".to_string());
    args
}

/// One group of destinations whose traffic a caller's interface delays.
///
/// Each band becomes one netem qdisc under the caller's `eth0` root `prio`
/// qdisc, with a u32 filter per destination steering packets into it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DelayBand {
    /// Added one-way latency, nanoseconds.
    pub delay_ns: u64,
    /// Random variation around the delay, nanoseconds.
    pub jitter_ns: u64,
    /// Backend addresses (after the VIP rewrite) whose traffic is delayed.
    pub destinations: Vec<SocketAddrV4>,
}

/// The most delay bands one interface can carry. A `prio` qdisc has at most
/// 16 bands, and the first three keep carrying ordinary traffic.
pub const MAX_DELAY_BANDS: usize = 13;

/// The delay bands each local caller's interface should carry, by instance.
///
/// `backends` maps a delay fault to its target's backend addresses; a fault
/// whose target has no backends yet delays nothing until it has some. A
/// caller outside every delay fault is absent from the result, which is how
/// the agent knows to take a previous qdisc away.
pub fn desired_delays<'a>(
    rules: impl IntoIterator<Item = &'a FaultRule>,
    backends: impl Fn(&FaultRule) -> Vec<SocketAddrV4>,
    callers: &[LocalCaller],
) -> BTreeMap<String, Vec<DelayBand>> {
    let mut rules: Vec<&FaultRule> = rules
        .into_iter()
        .filter(|rule| matches!(rule.fault_type, FaultType::Delay { .. }))
        .collect();
    // Band order follows fault ids, so the same faults always build the same
    // qdisc tree and an unchanged plan compares equal.
    rules.sort_by_key(|rule| rule.id.0);
    let mut desired: BTreeMap<String, Vec<DelayBand>> = BTreeMap::new();
    for rule in rules {
        let FaultType::Delay {
            delay_ns,
            jitter_ns,
            ..
        } = rule.fault_type
        else {
            continue;
        };
        let mut destinations = backends(rule);
        destinations.sort_unstable();
        destinations.dedup();
        if destinations.is_empty() {
            continue;
        }
        for caller in callers {
            if !applies_to_caller(rule, &caller.app, &caller.namespace) {
                continue;
            }
            let bands = desired.entry(caller.instance_id.clone()).or_default();
            if bands.len() < MAX_DELAY_BANDS {
                bands.push(DelayBand {
                    delay_ns,
                    jitter_ns,
                    destinations: destinations.clone(),
                });
            }
        }
    }
    desired
}

/// The interface inside a runc container that netem shapes.
pub const CONTAINER_INTERFACE: &str = "eth0";

/// Handle of the root `prio` qdisc Smoker installs. Anything else at the root
/// isn't ours, and Bun never deletes it.
pub const DELAY_ROOT_HANDLE: &str = "fa01:";

/// The `tc` argument lists that build `bands` on a container's interface,
/// in order. The caller removes any previous Smoker tree first.
///
/// The root is a `prio` qdisc with three ordinary bands (the default priomap
/// keeps every packet in them) plus one band per delay. Each delay band holds
/// a netem qdisc, and a u32 filter per destination (IPv4 address and TCP or
/// UDP destination port) steers matching packets into it.
pub fn delay_install_args(bands: &[DelayBand]) -> Vec<Vec<String>> {
    let text = |parts: &[&str]| parts.iter().map(|part| part.to_string()).collect();
    let mut commands: Vec<Vec<String>> = Vec::new();
    let band_count = (3 + bands.len()).to_string();
    let mut root: Vec<String> = text(&[
        "qdisc",
        "add",
        "dev",
        CONTAINER_INTERFACE,
        "root",
        "handle",
        DELAY_ROOT_HANDLE,
        "prio",
        "bands",
        &band_count,
        "priomap",
    ]);
    root.extend(
        [
            "1", "2", "2", "2", "1", "2", "0", "0", "1", "1", "1", "1", "1", "1", "1", "1",
        ]
        .map(str::to_string),
    );
    commands.push(root);
    for (index, band) in bands.iter().enumerate() {
        // Class minors are hexadecimal in tc's syntax; bands 1-3 are the
        // ordinary ones, so delays start at 4.
        let class = format!("{DELAY_ROOT_HANDLE}{:x}", 4 + index);
        let handle = format!("{:x}:", 0xfa10 + index);
        let mut netem = text(&[
            "qdisc",
            "add",
            "dev",
            CONTAINER_INTERFACE,
            "parent",
            &class,
            "handle",
            &handle,
            "netem",
            "delay",
            &format!("{}us", band.delay_ns / 1_000),
        ]);
        if band.jitter_ns > 0 {
            netem.push(format!("{}us", band.jitter_ns / 1_000));
        }
        commands.push(netem);
        for destination in &band.destinations {
            commands.push(text(&[
                "filter",
                "add",
                "dev",
                CONTAINER_INTERFACE,
                "parent",
                DELAY_ROOT_HANDLE,
                "protocol",
                "ip",
                "prio",
                "1",
                "u32",
                "match",
                "ip",
                "dst",
                &format!("{}/32", destination.ip()),
                "match",
                "ip",
                "dport",
                &destination.port().to_string(),
                "0xffff",
                "flowid",
                &class,
            ]));
        }
    }
    commands
}

/// The `tc` arguments that show the interface's root qdiscs.
pub fn delay_show_args() -> Vec<String> {
    ["qdisc", "show", "dev", CONTAINER_INTERFACE]
        .map(str::to_string)
        .to_vec()
}

/// The `tc` arguments that remove Smoker's tree and restore the interface's
/// default qdisc. Only run them when [`has_delay_root`] says the root is ours.
pub fn delay_remove_args() -> Vec<String> {
    ["qdisc", "del", "dev", CONTAINER_INTERFACE, "root"]
        .map(str::to_string)
        .to_vec()
}

/// Whether `tc qdisc show` output has Smoker's `prio` qdisc at the root.
pub fn has_delay_root(show_output: &str) -> bool {
    show_output.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("qdisc")
            && words.next() == Some("prio")
            && words.next() == Some(DELAY_ROOT_HANDLE)
            && words.next() == Some("root")
    })
}

/// The netem delays `tc qdisc show` reports under Smoker's root, e.g.
/// `["delay 300ms 20ms"]`, for the trace to show as live evidence.
pub fn installed_delays(show_output: &str) -> Vec<String> {
    show_output
        .lines()
        .filter(|line| {
            line.contains(" netem ") && line.contains(&format!("parent {DELAY_ROOT_HANDLE}"))
        })
        .filter_map(|line| {
            line.find("delay")
                .map(|start| line[start..].trim().to_string())
        })
        .collect()
}

/// Why a command in a container's network namespace failed.
#[derive(Debug, thiserror::Error)]
pub enum NetnsCommandError {
    /// The instance has no named network namespace (not a rootful runc
    /// container, or it has gone).
    #[error("instance {instance} has no network namespace")]
    NoNamespace { instance: String },
    /// The command could not be started.
    #[error("failed to run {program} in {instance}'s network namespace: {source}")]
    Spawn {
        instance: String,
        program: String,
        source: std::io::Error,
    },
    /// The command ran and failed.
    #[error("{program} in {instance}'s network namespace failed: {stderr}")]
    Failed {
        instance: String,
        program: String,
        stderr: String,
    },
    /// The command did not finish in time.
    #[error("{program} in {instance}'s network namespace timed out")]
    TimedOut { instance: String, program: String },
}

/// Run a host tool inside a runc container's network namespace
/// (`ip netns exec rb-<instance> <program> <args>`), returning its stdout.
///
/// The tool comes from the host, not the image, so a distroless container
/// gets the same treatment as a full one.
#[cfg(target_os = "linux")]
pub async fn run_in_instance_netns(
    instance_id: &str,
    program: &str,
    args: &[String],
) -> Result<String, NetnsCommandError> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let path =
        crate::grill::netns::namespace_path(&crate::grill::InstanceId(instance_id.to_string()));
    let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(NetnsCommandError::NoNamespace {
            instance: instance_id.to_string(),
        });
    };
    if !exists {
        return Err(NetnsCommandError::NoNamespace {
            instance: instance_id.to_string(),
        });
    }
    let output = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new("ip")
            .args(["netns", "exec", name, program])
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| NetnsCommandError::TimedOut {
        instance: instance_id.to_string(),
        program: program.to_string(),
    })?
    .map_err(|source| NetnsCommandError::Spawn {
        instance: instance_id.to_string(),
        program: program.to_string(),
        source,
    })?;
    if !output.status.success() {
        return Err(NetnsCommandError::Failed {
            instance: instance_id.to_string(),
            program: program.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

    fn address(text: &str) -> SocketAddrV4 {
        text.parse().unwrap()
    }

    #[test]
    fn only_a_new_key_or_a_changed_action_is_a_landing() {
        let drop = ConnectFaultEntry {
            action: ConnectFaultAction::Drop { probability: 50 },
            expires_ns: 10,
        };
        let later = ConnectFaultEntry {
            expires_ns: 20,
            ..drop
        };
        let partition = ConnectFaultEntry {
            action: ConnectFaultAction::Partition,
            expires_ns: 20,
        };
        assert!(lands(None, &drop));
        assert!(!lands(Some(&drop), &later));
        assert!(lands(Some(&drop), &partition));
    }

    #[test]
    fn a_wildcard_key_cuts_every_caller_and_a_scoped_key_only_its_cgroup() {
        let callers = [
            caller("default/frontend-0", "frontend", "default", Some(11)),
            caller("default/worker-0", "worker", "default", None),
        ];
        let backends = |_vip: u32, _port: u16| vec![address("10.1.0.5:6379")];

        let everyone = connections_to_cut(&[key(0)], &callers, backends);
        assert_eq!(
            everyone
                .iter()
                .map(|cut| cut.instance_id.as_str())
                .collect::<Vec<_>>(),
            vec!["default/frontend-0", "default/worker-0"]
        );

        let scoped = connections_to_cut(&[key(11)], &callers, backends);
        assert_eq!(
            scoped,
            vec![ConnectionCut {
                instance_id: "default/frontend-0".to_string(),
                backends: vec![address("10.1.0.5:6379")],
            }]
        );
        assert!(connections_to_cut(&[key(11)], &callers, |_, _| Vec::new()).is_empty());
    }

    #[test]
    fn socket_destroy_arguments_match_any_backend() {
        assert_eq!(
            socket_destroy_args(&[address("10.1.0.5:6379"), address("192.168.5.2:30001")]),
            [
                "-K",
                "-tn",
                "state",
                "established",
                "(",
                "dst",
                "10.1.0.5:6379",
                "or",
                "dst",
                "192.168.5.2:30001",
                ")",
            ]
        );
    }

    #[test]
    fn a_delay_from_one_source_shapes_only_that_source_towards_the_backends() {
        let delay = rule(
            3,
            FaultType::Delay {
                delay_ns: 300_000_000,
                jitter_ns: 0,
                source_app: Some("frontend".to_string()),
            },
        );
        let callers = [
            caller("default/frontend-0", "frontend", "default", None),
            caller("default/backend-0", "backend", "default", None),
        ];
        let backends = |_: &FaultRule| {
            vec![
                address("10.1.0.9:6379"),
                address("10.1.0.5:6379"),
                address("10.1.0.5:6379"),
            ]
        };
        let desired = desired_delays([&delay], backends, &callers);
        assert_eq!(
            desired,
            BTreeMap::from([(
                "default/frontend-0".to_string(),
                vec![DelayBand {
                    delay_ns: 300_000_000,
                    jitter_ns: 0,
                    destinations: vec![address("10.1.0.5:6379"), address("10.1.0.9:6379")],
                }],
            )])
        );
        // No backends yet: nothing to delay, so no qdisc at all.
        assert!(desired_delays([&delay], |_| Vec::new(), &callers).is_empty());
    }

    #[test]
    fn delay_bands_follow_fault_ids_whatever_the_registry_order() {
        let fault = |id, delay_ns| {
            rule(
                id,
                FaultType::Delay {
                    delay_ns,
                    jitter_ns: 0,
                    source_app: None,
                },
            )
        };
        let (first, second) = (fault(1, 100), fault(2, 200));
        let callers = [caller("default/web-0", "web", "default", None)];
        let backends = |_: &FaultRule| vec![address("10.1.0.5:80")];
        assert_eq!(
            desired_delays([&second, &first], backends, &callers),
            desired_delays([&first, &second], backends, &callers)
        );
    }

    #[test]
    fn delay_install_builds_a_prio_root_a_netem_band_and_a_filter_per_backend() {
        let commands = delay_install_args(&[DelayBand {
            delay_ns: 300_000_000,
            jitter_ns: 20_000_000,
            destinations: vec![address("10.1.0.5:6379"), address("10.1.0.9:6379")],
        }]);
        let joined: Vec<String> = commands.iter().map(|args| args.join(" ")).collect();
        assert_eq!(
            joined,
            vec![
                "qdisc add dev eth0 root handle fa01: prio bands 4 priomap 1 2 2 2 1 2 0 0 1 1 1 1 1 1 1 1",
                "qdisc add dev eth0 parent fa01:4 handle fa10: netem delay 300000us 20000us",
                "filter add dev eth0 parent fa01: protocol ip prio 1 u32 match ip dst 10.1.0.5/32 match ip dport 6379 0xffff flowid fa01:4",
                "filter add dev eth0 parent fa01: protocol ip prio 1 u32 match ip dst 10.1.0.9/32 match ip dport 6379 0xffff flowid fa01:4",
            ]
        );
    }

    #[test]
    fn a_tenth_band_uses_hexadecimal_class_minors() {
        let band = DelayBand {
            delay_ns: 1_000_000,
            jitter_ns: 0,
            destinations: vec![address("10.1.0.5:80")],
        };
        let commands = delay_install_args(&vec![band; 10]);
        let last_netem = commands
            .iter()
            .rfind(|args| args.contains(&"netem".to_string()))
            .unwrap()
            .join(" ");
        assert!(
            last_netem.contains("parent fa01:d handle fa19:"),
            "{last_netem}"
        );
    }

    #[test]
    fn only_smokers_own_root_is_recognised_and_its_delays_read_back() {
        let ours = "qdisc prio fa01: root refcnt 5 bands 4 priomap 1 2 2 2 1 2 0 0 1 1 1 1 1 1 1 1\n\
                    qdisc netem fa10: parent fa01:4 limit 1000 delay 300ms  20ms\n";
        assert!(has_delay_root(ours));
        assert_eq!(installed_delays(ours), vec!["delay 300ms  20ms"]);
        assert!(!has_delay_root("qdisc noqueue 0: root refcnt 2\n"));
        assert!(!has_delay_root("qdisc prio 1: root refcnt 2 bands 3\n"));
        assert!(installed_delays("qdisc noqueue 0: root refcnt 2\n").is_empty());
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
