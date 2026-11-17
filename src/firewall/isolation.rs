//! Namespace isolation on the forwarding path.
//!
//! The eBPF socket hooks hold every `connect()` and `sendmsg()` to namespace
//! isolation, but some traffic never meets a socket hook: raw sockets, and a
//! container dialling any address at a published host port, which the
//! `reliaburger` table's DNAT then forwards to another container on this
//! node. This table checks what the hooks can't see, after DNAT, on the
//! forward path between host veths (`veth-…`): a packet from one local
//! workload to another passes only when both share a namespace, when the
//! destination's `allow_from` grants that source, or when it answers a
//! connection that was already allowed.
//!
//! Traffic from outside the node, from host processes, and from a container
//! address this node doesn't know is untouched here; the socket hooks and
//! the perimeter table own those paths.
//!
//! Uses its own `reliaburger_isolation` table, rebuilt whole in one `nft -f`
//! transaction every time it changes.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

/// The nftables table holding the isolation rules.
pub const TABLE: &str = "reliaburger_isolation";

/// The local workload addresses and grants the forward path enforces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IsolationPlan {
    /// Each local workload address, by namespace id.
    pub namespaces: BTreeMap<u32, BTreeSet<Ipv4Addr>>,
    /// `(source, destination)` workload address pairs that `allow_from`
    /// opens across namespaces.
    pub granted: BTreeSet<(Ipv4Addr, Ipv4Addr)>,
}

impl IsolationPlan {
    /// Every workload address in the plan.
    fn workloads(&self) -> BTreeSet<Ipv4Addr> {
        self.namespaces.values().flatten().copied().collect()
    }
}

fn element_list<T: std::fmt::Display>(elements: impl IntoIterator<Item = T>) -> Option<String> {
    let rendered: Vec<String> = elements.into_iter().map(|e| e.to_string()).collect();
    (!rendered.is_empty()).then(|| format!("        elements = {{ {} }}\n", rendered.join(", ")))
}

/// Render the whole table. Deleting and redefining it in one script makes
/// the swap atomic: `nft -f` applies a script as one transaction.
pub fn generate_ruleset(plan: &IsolationPlan) -> String {
    let mut sets = String::new();
    let mut accepts = String::new();
    sets.push_str("    set workloads {\n        type ipv4_addr\n");
    sets.push_str(&element_list(plan.workloads()).unwrap_or_default());
    sets.push_str("    }\n");
    sets.push_str("    set granted {\n        type ipv4_addr . ipv4_addr\n");
    sets.push_str(
        &element_list(
            plan.granted
                .iter()
                .map(|(source, destination)| format!("{source} . {destination}")),
        )
        .unwrap_or_default(),
    );
    sets.push_str("    }\n");
    for (namespace, addresses) in &plan.namespaces {
        let name = format!("ns_{namespace:08x}");
        sets.push_str(&format!("    set {name} {{\n        type ipv4_addr\n"));
        sets.push_str(&element_list(addresses).unwrap_or_default());
        sets.push_str("    }\n");
        accepts.push_str(&format!(
            "        ip saddr @{name} ip daddr @{name} accept\n"
        ));
    }
    format!(
        "table ip {TABLE}\n\
         delete table ip {TABLE}\n\
         table ip {TABLE} {{\n\
         {sets}\
         \x20   chain forward {{\n\
         \x20       type filter hook forward priority -5; policy accept;\n\
         \x20       iifname != \"veth-*\" accept\n\
         \x20       ct state established,related accept\n\
         \x20       ip daddr != @workloads accept\n\
         \x20       ip saddr . ip daddr @granted accept\n\
         {accepts}\
         \x20       ip saddr @workloads drop\n\
         \x20   }}\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> IsolationPlan {
        IsolationPlan {
            namespaces: BTreeMap::from([
                (
                    1,
                    BTreeSet::from([Ipv4Addr::new(10, 88, 0, 2), Ipv4Addr::new(10, 88, 0, 3)]),
                ),
                (2, BTreeSet::from([Ipv4Addr::new(10, 88, 0, 4)])),
            ]),
            granted: BTreeSet::from([(Ipv4Addr::new(10, 88, 0, 4), Ipv4Addr::new(10, 88, 0, 2))]),
        }
    }

    #[test]
    fn the_table_is_replaced_in_one_transaction() {
        let rules = generate_ruleset(&plan());
        let lines: Vec<&str> = rules.lines().collect();
        assert_eq!(lines[0], "table ip reliaburger_isolation");
        assert_eq!(lines[1], "delete table ip reliaburger_isolation");
    }

    #[test]
    fn workloads_reach_their_own_namespace_and_granted_destinations_only() {
        let rules = generate_ruleset(&plan());
        assert!(rules.contains("elements = { 10.88.0.2, 10.88.0.3, 10.88.0.4 }"));
        assert!(rules.contains("ip saddr @ns_00000001 ip daddr @ns_00000001 accept"));
        assert!(rules.contains("ip saddr @ns_00000002 ip daddr @ns_00000002 accept"));
        assert!(rules.contains("elements = { 10.88.0.4 . 10.88.0.2 }"));
        // Only traffic between host veths, towards a known workload, from a
        // known workload, is ever dropped.
        let drop = rules.find("ip saddr @workloads drop").unwrap();
        for guard in [
            "iifname != \"veth-*\" accept",
            "ct state established,related accept",
            "ip daddr != @workloads accept",
            "ip saddr . ip daddr @granted accept",
            "ip saddr @ns_00000002 ip daddr @ns_00000002 accept",
        ] {
            assert!(rules.find(guard).unwrap() < drop, "{guard} must come first");
        }
        assert!(rules.contains("policy accept"));
    }

    #[test]
    fn an_empty_plan_renders_empty_sets_nft_accepts() {
        let rules = generate_ruleset(&IsolationPlan::default());
        assert!(
            !rules.contains("elements"),
            "nft refuses an empty element list"
        );
        assert!(rules.contains("set workloads {\n        type ipv4_addr\n    }"));
    }
}
