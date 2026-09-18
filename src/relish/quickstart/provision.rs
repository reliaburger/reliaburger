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
    Ok(toml::to_string_pretty(&config)?)
}

/// Guest service supervised and restarted by systemd; logs go to its journal.
pub const SERVICE: &str = "[Unit]\nDescription=Reliaburger node\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart=/usr/local/bin/bun --cluster --runtime runc --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117\nRestart=on-failure\nRestartSec=2\nLimitNOFILE=1048576\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n";

#[cfg(test)]
mod tests {
    use super::*;

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
}
