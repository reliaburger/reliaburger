//! What `relish setup --ssh` asks of a machine, before it changes anything.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{Result, bail};

use super::ssh::SshTarget;

const GIB_IN_KIB: u64 = 1024 * 1024;
/// A "2 GiB" VM reports a little less than 2 GiB, because the kernel keeps some.
const MINIMUM_MEMORY_KIB: u64 = 1900 * 1024;
const MINIMUM_DISK_KIB: u64 = 10 * GIB_IN_KIB;
const MINIMUM_CPUS: u32 = 2;
const MINIMUM_KERNEL: (u32, u32) = (5, 8);

/// The packages a node needs, from `docs/linux-servers.md`.
pub const PACKAGES: [&str; 7] = [
    "runc",
    "uidmap",
    "iptables",
    "iproute2",
    "btrfs-progs",
    "nftables",
    "curl",
];

/// Reports what [`Facts`] needs as `key=value` lines. It runs as the login
/// user, which is what lets it see `SSH_CLIENT`, and changes nothing.
pub const PREFLIGHT: &str = r#"[ -r /etc/os-release ] && . /etc/os-release
echo "os=${ID:-unknown}"
echo "kernel=$(uname -r)"
echo "arch=$(uname -m)"
echo "cpus=$(nproc 2>/dev/null)"
echo "memory_kib=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null)"
echo "disk_kib=$(df -Pk /var/lib 2>/dev/null | awk 'NR==2 {print $4}')"
[ -f /sys/fs/cgroup/cgroup.controllers ] && echo cgroup2=yes
[ -d /run/systemd/system ] && echo systemd=yes
if sudo -n true 2>/dev/null; then
  echo sudo=yes
  sudo -n test -e /etc/reliaburger && echo used=yes
  sudo -n ufw status 2>/dev/null | grep -q '^Status: active' && echo ufw=yes
  sudo -n iptables -S FORWARD 2>/dev/null | grep -q '^-P FORWARD DROP' && echo forward_drop=yes
fi
echo "addresses=$(hostname -I 2>/dev/null)"
echo "operator=${SSH_CLIENT%% *}"
"#;

/// What a machine reported about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    os: String,
    kernel: String,
    /// CPU architecture, in release naming: `x86_64` or `aarch64`.
    pub arch: String,
    cpus: u32,
    memory_kib: u64,
    disk_kib: u64,
    cgroup2: bool,
    systemd: bool,
    sudo: bool,
    used: bool,
    ufw: bool,
    forward_drop: bool,
    addresses: Vec<Ipv4Addr>,
    /// The address this machine sees the operator connecting from.
    pub operator: Option<Ipv4Addr>,
}

impl Facts {
    /// Read the output of [`PREFLIGHT`].
    pub fn parse(report: &str) -> Self {
        let values: HashMap<&str, &str> = report
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let text = |key: &str| values.get(key).copied().unwrap_or_default().trim();
        let flag = |key: &str| text(key) == "yes";
        let arch = match text("arch") {
            "amd64" => "x86_64",
            "arm64" => "aarch64",
            other => other,
        };
        Self {
            os: text("os").to_owned(),
            kernel: text("kernel").to_owned(),
            arch: arch.to_owned(),
            cpus: text("cpus").parse().unwrap_or(0),
            memory_kib: text("memory_kib").parse().unwrap_or(0),
            disk_kib: text("disk_kib").parse().unwrap_or(0),
            cgroup2: flag("cgroup2"),
            systemd: flag("systemd"),
            sudo: flag("sudo"),
            used: flag("used"),
            ufw: flag("ufw"),
            forward_drop: flag("forward_drop"),
            addresses: text("addresses")
                .split_whitespace()
                .filter_map(|word| word.parse().ok())
                .collect(),
            operator: text("operator").parse().ok(),
        }
    }

    /// Everything that stops this machine becoming a node.
    ///
    /// `resuming` is true once this operation has started changing the
    /// machine, when `/etc/reliaburger` is its own work rather than someone's.
    pub fn problems(&self, resuming: bool) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.sudo {
            problems.push("needs passwordless sudo for the login user".to_owned());
        }
        if !matches!(self.os.as_str(), "ubuntu" | "debian") {
            problems.push(format!(
                "runs {:?}; only Ubuntu and Debian are supported",
                self.os
            ));
        }
        if !matches!(self.arch.as_str(), "x86_64" | "aarch64") {
            problems.push(format!(
                "is {:?}; releases are x86_64 and aarch64",
                self.arch
            ));
        }
        if !kernel_is_supported(&self.kernel) {
            problems.push(format!(
                "runs kernel {:?}; 5.8 or later is needed",
                self.kernel
            ));
        }
        if !self.cgroup2 {
            problems.push("doesn't have cgroup v2 enabled".to_owned());
        }
        if !self.systemd {
            problems.push("doesn't run systemd".to_owned());
        }
        if self.cpus < MINIMUM_CPUS {
            problems.push(format!("has {} CPU cores; 2 are needed", self.cpus));
        }
        if self.memory_kib < MINIMUM_MEMORY_KIB {
            problems.push(format!(
                "has {} MiB of memory; 2 GiB is needed",
                self.memory_kib / 1024
            ));
        }
        if self.disk_kib < MINIMUM_DISK_KIB {
            problems.push(format!(
                "has {} GiB free under /var/lib; 10 GiB is needed",
                self.disk_kib / GIB_IN_KIB
            ));
        }
        if self.used && !resuming {
            problems.push(
                "already has /etc/reliaburger; refusing to overwrite a machine this setup didn't start"
                    .to_owned(),
            );
        }
        problems
    }

    /// Things that won't stop setup but will stop traffic, with the way out.
    pub fn warnings(&self, peers: &[Ipv4Addr]) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.ufw {
            let peers: Vec<String> = peers.iter().map(Ipv4Addr::to_string).collect();
            warnings.push(format!(
                "UFW is active; nodes won't reach each other until you allow {} on the cluster's \
                 ports (docs/linux-servers.md, section 1)",
                peers.join(", ")
            ));
        }
        if self.forward_drop {
            warnings.push(
                "the FORWARD policy is DROP, usually from Docker or UFW, which drops traffic \
                 between containers (docs/README.md, runc notes)"
                    .to_owned(),
            );
        }
        warnings
    }

    /// The address peers should use for this machine.
    ///
    /// The address we reached it on can be a public one or a port forward, so
    /// it only counts if the machine says it's its own. Otherwise a machine
    /// with one private address has an obvious answer; anything else is a
    /// guess about someone's network, which `--advertise` settles instead.
    pub fn cluster_address(&self, target: &SshTarget) -> Result<Ipv4Addr> {
        if let Ok(address) = target.host.parse::<Ipv4Addr>()
            && self.addresses.contains(&address)
        {
            return Ok(address);
        }
        let private: Vec<_> = self
            .addresses
            .iter()
            .filter(|address| address.is_private() && !is_docker_bridge(**address))
            .collect();
        match private.as_slice() {
            [address] => Ok(**address),
            [] => bail!("{target} has no private address; name one with --advertise"),
            _ => bail!("{target} has several private addresses; choose one with --advertise"),
        }
    }
}

fn is_docker_bridge(address: Ipv4Addr) -> bool {
    address.octets()[..2] == [172, 17]
}

fn kernel_is_supported(release: &str) -> bool {
    let mut parts = release.split(['.', '-']).map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= MINIMUM_KERNEL,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "os=ubuntu\nkernel=6.8.0-45-generic\narch=aarch64\ncpus=4\n\
        memory_kib=8000000\ndisk_kib=40000000\ncgroup2=yes\nsystemd=yes\nsudo=yes\n\
        addresses=192.168.5.15 10.0.0.1 172.17.0.1\noperator=10.0.0.50\n";

    fn with(line: &str) -> Facts {
        Facts::parse(&format!("{GOOD}{line}\n"))
    }

    fn without(key: &str) -> Facts {
        let report: Vec<_> = GOOD
            .lines()
            .filter(|line| !line.starts_with(&format!("{key}=")))
            .collect();
        Facts::parse(&report.join("\n"))
    }

    fn target(text: &str) -> SshTarget {
        text.parse().unwrap()
    }

    #[test]
    fn a_suitable_machine_has_no_problems() {
        assert_eq!(Facts::parse(GOOD).problems(false), Vec::<String>::new());
    }

    #[test]
    fn each_shortfall_is_reported_and_all_are_reported_together() {
        for (line, expected) in [
            ("os=fedora", "only Ubuntu and Debian"),
            ("arch=riscv64", "releases are"),
            ("kernel=5.4.0-1-generic", "5.8 or later"),
            ("cpus=1", "2 are needed"),
            ("memory_kib=1000000", "2 GiB is needed"),
            ("disk_kib=1000", "10 GiB is needed"),
            ("used=yes", "already has /etc/reliaburger"),
        ] {
            let problems = with(line).problems(false);
            assert!(
                problems.iter().any(|problem| problem.contains(expected)),
                "{line}: {problems:?}"
            );
        }
        for (key, expected) in [
            ("sudo", "passwordless sudo"),
            ("cgroup2", "cgroup v2"),
            ("systemd", "systemd"),
        ] {
            let problems = without(key).problems(false);
            assert!(problems.iter().any(|problem| problem.contains(expected)));
        }
        assert_eq!(Facts::parse("").problems(false).len(), 9);
    }

    #[test]
    fn a_two_gib_vm_is_enough_despite_what_the_kernel_keeps() {
        assert!(with("memory_kib=2014000").problems(false).is_empty());
    }

    #[test]
    fn a_machine_this_setup_started_can_be_resumed() {
        assert!(with("used=yes").problems(true).is_empty());
    }

    #[test]
    fn kernels_compare_as_numbers() {
        for good in ["5.8", "5.10.0-28-amd64", "6.8.0-45-generic"] {
            assert!(kernel_is_supported(good), "{good}");
        }
        for bad in ["5.7.19", "4.19.0", "", "unknown"] {
            assert!(!kernel_is_supported(bad), "{bad}");
        }
    }

    #[test]
    fn firewalls_warn_with_a_way_out_but_dont_stop_setup() {
        let peers: [Ipv4Addr; 1] = ["10.0.0.2".parse().unwrap()];
        assert!(Facts::parse(GOOD).warnings(&peers).is_empty());
        let facts = with("ufw=yes\nforward_drop=yes");
        assert!(facts.problems(false).is_empty());
        let warnings = facts.warnings(&peers);
        assert!(warnings[0].contains("10.0.0.2"));
        assert!(warnings[1].contains("FORWARD"));
    }

    #[test]
    fn a_target_the_machine_owns_is_its_cluster_address() {
        let facts = Facts::parse(GOOD);
        let address = facts.cluster_address(&target("u@10.0.0.1")).unwrap();
        assert_eq!(address, "10.0.0.1".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn a_public_or_forwarded_target_falls_back_to_the_only_private_address() {
        let facts = with("addresses=203.0.113.9 10.0.0.1 172.17.0.1");
        for target_text in ["u@203.0.113.200", "u@node-1.example.com"] {
            let address = facts.cluster_address(&target(target_text)).unwrap();
            assert_eq!(address, "10.0.0.1".parse::<Ipv4Addr>().unwrap());
        }
    }

    #[test]
    fn several_private_addresses_are_a_question_for_the_operator() {
        let error = Facts::parse(GOOD)
            .cluster_address(&target("u@node-1"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--advertise"), "{error}");
        let error = with("addresses=203.0.113.9")
            .cluster_address(&target("u@node-1"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--advertise"), "{error}");
    }

    #[test]
    fn the_operator_is_read_from_the_ssh_session() {
        let operator = Facts::parse(GOOD).operator;
        assert_eq!(operator, Some("10.0.0.50".parse().unwrap()));
        assert_eq!(without("operator").operator, None);
    }

    /// The real script, run by a real shell, against the parser.
    #[test]
    fn the_preflight_script_runs_and_changes_nothing_it_cant() {
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", PREFLIGHT])
            .env("SSH_CLIENT", "10.0.0.50 5000 22")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let facts = Facts::parse(&String::from_utf8(output.stdout).unwrap());
        assert_eq!(facts.operator, Some("10.0.0.50".parse().unwrap()));
        assert!(!facts.arch.is_empty());
    }
}
