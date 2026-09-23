//! Reproducible VM and node configuration for the managed laptop cluster.

use std::net::Ipv4Addr;

use crate::config::node::NodeConfig;
use anyhow::{Result, bail};

/// Generate an isolated VM using an already verified local Ubuntu image.
pub fn vm_config(
    image: &str,
    arch: &str,
    api_port: u16,
    ingress_port: Option<u16>,
    registry_port: Option<u16>,
) -> Result<String> {
    if !matches!(arch, "aarch64" | "x86_64") {
        bail!("unsupported VM architecture");
    }
    let mut forwards = vec![serde_json::json!({
        "guestPort":9117, "hostPort":api_port, "hostIP":"127.0.0.1"
    })];
    if let Some(port) = ingress_port {
        forwards.push(serde_json::json!({"guestPort":80,"hostPort":port,"hostIP":"127.0.0.1"}));
    }
    if let Some(port) = registry_port {
        forwards.push(serde_json::json!({"guestPort":5050,"hostPort":port,"hostIP":"127.0.0.1"}));
    }
    // Lima otherwise forwards every listening guest port automatically.
    forwards.push(serde_json::json!({
        "guestPortRange":[1,65535], "guestIP":"0.0.0.0", "proto":"any", "ignore":true
    }));
    let value = serde_json::json!({
        "vmType": if cfg!(target_os="macos") {"vz"} else {"qemu"},
        "arch":arch,
        "images":[{"location":image,"arch":arch}],
        "cpus":2,"memory":"2GiB","disk":"10GiB",
        "mounts":[],
        "containerd":{"system":false,"user":false},
        "networks":[{"lima":"user-v2"}],
        "portForwards":forwards,
        "provision":[{"mode":"system","script": concat!(
            "#!/bin/bash\nset -eu\nexport DEBIAN_FRONTEND=noninteractive\n",
            // Lima changes the user manager during first boot. Reconnect logind
            // after that transition so subsequent PAM sessions do not stall.
            // A graceful stop sometimes spins until systemd's 90 s stop
            // timeout kills it, stalling boot. Kill it straight away instead.
            "systemctl kill --signal=SIGKILL systemd-logind.service || true\n",
            "systemctl restart systemd-logind.service\n",
            "apt-get update -qq\napt-get install -y -qq runc uidmap btrfs-progs nftables iptables iproute2\n",
            "install -d -m 700 /etc/reliaburger\n")
        }]
    });
    Ok(serde_yaml::to_string(&value)?)
}

/// Generate a node config with pinned identity paths and authenticated transport.
pub fn node_config(
    cluster: &str,
    name: &str,
    address: Ipv4Addr,
    seed: Option<Ipv4Addr>,
    peers: &[Ipv4Addr],
) -> Result<String> {
    let mut config = NodeConfig::default();
    config.node.name = Some(name.to_owned());
    config.cluster.name = cluster.to_owned();
    config.cluster.validate()?;
    config.cluster.join = seed.into_iter().map(|ip| format!("{ip}:9443")).collect();
    config.network.advertise_address = Some(address.to_string());
    config.security.require_mtls = true;
    config.security.bootstrap_peers = peers.iter().copied().map(std::net::IpAddr::V4).collect();
    config.security.allow_insecure_cluster = false;
    config.security.identity_dir = Some("/etc/reliaburger/identity".into());
    config.security.master_key_path = Some("/etc/reliaburger/master.key".into());
    if seed.is_none() {
        config.security.bootstrap_path = Some("/etc/reliaburger/security-bootstrap.json".into());
    }
    config.ebpf.enabled = true;
    config.dns.enabled = true;
    config.dns.listen = format!("{address}:53");
    config.ingress.enabled = true;
    config.testing = laptop_test_policy();
    Ok(toml::to_string_pretty(&config)?)
}

/// The fault policy every quickstart node serves (decision D3).
///
/// A laptop cluster is a throwaway development cluster, and breaking it on
/// purpose is half the point of having one. So it admits workload faults
/// (kill, pause, CPU, memory, network) and node faults (`node-kill`,
/// `node-drain`), which the quorum and leader rails still guard and which
/// expire on their own. It leaves out node pressure, which could starve a
/// 2 GiB VM's own control plane, external trace probes, and isolated test
/// workloads. Server installs keep the protected default: a missing
/// `[testing]` section still means `unknown`, which allows nothing.
pub fn laptop_test_policy() -> crate::testkit::safety::ClusterTestPolicy {
    use crate::testkit::safety::{ClusterSafetyClass, ClusterTestPolicy, OperationPermission};
    ClusterTestPolicy {
        safety_class: ClusterSafetyClass::Development,
        allowed_operations: [
            OperationPermission::InjectWorkloadFaults,
            OperationPermission::AlterNodeState,
        ]
        .into(),
        ..ClusterTestPolicy::default()
    }
}

/// One line describing a node's live fault policy, for `relish local status`.
pub fn describe_test_policy(policy: &crate::testkit::safety::ClusterTestPolicy) -> String {
    let class = serde_json::to_value(policy.safety_class)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    if policy.allowed_operations.is_empty() {
        return format!("fault policy: {class}; faults are refused");
    }
    let operations: Vec<String> = policy
        .allowed_operations
        .iter()
        .map(ToString::to_string)
        .collect();
    format!("fault policy: {class}; allows {}", operations.join(", "))
}

/// Guest service supervised and restarted by systemd; logs go to its journal.
pub const SERVICE: &str = "[Unit]\nDescription=Reliaburger node\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStartPre=/bin/sh -ec 'mountpoint -q /sys/fs/bpf || mount -t bpf bpf /sys/fs/bpf'\nExecStart=/usr/local/bin/bun --cluster --runtime runc --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117\nRestart=on-failure\nRestartSec=2\nLimitNOFILE=1048576\nKillMode=process\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_restarts_leave_durable_owners_running() {
        // Owners outlive Bun by design; `mixed` or `control-group` would
        // SIGKILL them on every restart and leave their records unowned.
        assert!(SERVICE.contains("\nKillMode=process\n"), "{SERVICE}");
        assert!(!SERVICE.contains("KillMode=mixed"));
    }

    #[test]
    fn managed_vm_has_no_host_mounts_and_only_explicit_loopback_forwards() {
        let yaml = vm_config(
            "/private/cache/ubuntu.img",
            "aarch64",
            19117,
            Some(18080),
            Some(15050),
        )
        .unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(value["mounts"].as_sequence().unwrap().len(), 0);
        assert_eq!(value["containerd"]["user"], false);
        assert_eq!(value["containerd"]["system"], false);
        let forwards = value["portForwards"].as_sequence().unwrap();
        assert_eq!(forwards.len(), 4);
        assert_eq!(forwards[2]["guestPort"], 5050);
        assert_eq!(forwards[2]["hostPort"], 15050);
        assert_eq!(forwards[2]["hostIP"], "127.0.0.1");
        assert_eq!(forwards[0]["hostIP"], "127.0.0.1");
        assert_eq!(forwards[0]["hostPort"], 19117);
        assert_eq!(forwards[1]["hostPort"], 18080);
        assert_eq!(forwards[3]["ignore"], true);
        assert_eq!(forwards[3]["proto"], "any");
        assert_eq!(forwards[3]["guestIP"], "0.0.0.0");
        assert_eq!(value["networks"][0]["lima"], "user-v2");
    }

    #[test]
    fn provisioning_never_waits_for_a_graceful_logind_stop() {
        let yaml = vm_config("/private/cache/ubuntu.img", "aarch64", 19117, None, None).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let script = value["provision"][0]["script"].as_str().unwrap();
        let kill = script
            .find("systemctl kill --signal=SIGKILL systemd-logind.service")
            .unwrap();
        let restart = script
            .find("systemctl restart systemd-logind.service")
            .unwrap();
        assert!(kill < restart, "{script}");
    }

    #[test]
    fn node_config_enforces_authenticated_cluster_and_linux_dataplane() {
        let first = node_config(
            "laptop",
            "rb-laptop-123-1",
            "192.168.104.2".parse().unwrap(),
            None,
            &["192.168.104.3".parse().unwrap()],
        )
        .unwrap();
        let node: crate::config::node::NodeConfig = toml::from_str(&first).unwrap();
        assert!(node.security.require_mtls);
        assert_eq!(
            node.security.bootstrap_peers,
            vec!["192.168.104.3".parse::<std::net::IpAddr>().unwrap()]
        );
        assert!(!node.security.allow_insecure_cluster);
        assert!(node.security.bootstrap_path.is_some());
        assert!(node.ebpf.enabled);
        assert!(node.dns.enabled);
        assert_eq!(node.dns.listen, "192.168.104.2:53");
        assert!(node.ingress.enabled);
        let peer = node_config(
            "laptop",
            "rb-laptop-123-2",
            "192.168.104.3".parse().unwrap(),
            Some("192.168.104.2".parse().unwrap()),
            &["192.168.104.2".parse().unwrap()],
        )
        .unwrap();
        let node: crate::config::node::NodeConfig = toml::from_str(&peer).unwrap();
        assert!(node.security.bootstrap_path.is_none());
        assert_eq!(node.cluster.join, vec!["192.168.104.2:9443"]);
    }

    #[test]
    fn laptop_nodes_admit_workload_and_node_faults_but_not_pressure() {
        use crate::sesame::types::ApiRole;
        use crate::testkit::safety::{OperationAuthorisation, OperationPermission};

        let text = node_config(
            "laptop",
            "rb-laptop-123-1",
            "192.168.104.2".parse().unwrap(),
            None,
            &[],
        )
        .unwrap();
        assert!(text.contains("[testing]"), "{text}");
        assert!(text.contains("safety_class = \"development\""), "{text}");
        let node: crate::config::node::NodeConfig = toml::from_str(&text).unwrap();
        node.testing.validate().unwrap();
        let admin = OperationAuthorisation {
            principal: "laptop",
            role: ApiRole::Admin,
            acknowledged: true,
        };
        for allowed in [
            OperationPermission::InjectWorkloadFaults,
            OperationPermission::AlterNodeState,
        ] {
            assert!(node.testing.authorise(allowed, &admin).is_ok(), "{allowed}");
        }
        for refused in [
            OperationPermission::SaturateCapacity,
            OperationPermission::ProbeExternalDestination,
            OperationPermission::ProvisionIsolatedWorkloads,
        ] {
            assert!(
                node.testing.authorise(refused, &admin).is_err(),
                "{refused}"
            );
        }
        // Consent is still required: the policy grants permission, not intent.
        let unacknowledged = OperationAuthorisation {
            acknowledged: false,
            ..admin
        };
        assert!(
            node.testing
                .authorise(OperationPermission::InjectWorkloadFaults, &unacknowledged)
                .is_err()
        );
    }

    #[test]
    fn local_status_describes_the_live_fault_policy() {
        assert_eq!(
            describe_test_policy(&laptop_test_policy()),
            "fault policy: development; allows inject_workload_faults, alter_node_state"
        );
        assert_eq!(
            describe_test_policy(&crate::testkit::safety::ClusterTestPolicy::default()),
            "fault policy: unknown; faults are refused"
        );
    }
}
