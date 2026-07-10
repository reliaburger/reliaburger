//! nftables named-map port mapping (Phase 12, slice A).
//!
//! Replaces the per-container DNAT rules with a single named map:
//! one lookup rule in the `prerouting` chain consults `@portmap`
//! (host port → container IP . container port), so the kernel does an
//! O(1) hash lookup instead of walking one rule per container.
//!
//! Everything here is either a pure argv generator (unit-testable
//! anywhere) or generic over [`NftExecutor`], so the logic runs under
//! a recording mock on macOS and against the real `nft` binary on
//! Linux. All argv vectors keep `{ ... }` blocks as a single element —
//! `nft` joins argv itself, and building shell strings invites quoting
//! bugs.

use std::collections::HashSet;
use std::net::Ipv4Addr;

/// Errors from map-based port mapping. The Linux netns layer wraps
/// these in its own `NetnsError` at the boundary; keeping a separate
/// enum lets this module (and its tests) build on every platform.
#[derive(Debug, thiserror::Error)]
pub enum PortMapError {
    #[error("failed to map host port {host_port} -> {container_port}: {reason}")]
    ApplyFailed {
        host_port: u16,
        container_port: u16,
        reason: String,
    },

    #[error("failed to remove mapping for host port {host_port}: {reason}")]
    RemoveFailed { host_port: u16, reason: String },
}

/// The nftables table holding container NAT state. Deliberately
/// distinct from the perimeter firewall's `reliaburger_fw` table,
/// whose reconcile deletes and recreates its own table wholesale.
const TABLE: &str = "reliaburger";

/// The named map: host port → container address . container port.
const MAP: &str = "portmap";

/// Executes an nft command given as argv (without the leading `nft`).
///
/// Two implementations exist: the production one shells out to the
/// `nft` binary, and the test one records argv and returns scripted
/// results. The error is the human-readable reason (stderr or exec
/// failure); callers wrap it in a [`NetnsError`].
pub trait NftExecutor: Send + Sync {
    fn run(&self, args: &[String]) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// One host-port → container mapping, the map's key/value pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMapEntry {
    pub host_port: u16,
    pub container_ip: Ipv4Addr,
    pub container_port: u16,
}

/// Argv for creating the named map (idempotence is the caller's job:
/// probe with `nft list map` first, create on failure).
pub fn portmap_definition() -> Vec<String> {
    [
        "add",
        "map",
        "ip",
        TABLE,
        MAP,
        "{ type inet_service : ipv4_addr . inet_service ; }",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Argv for the single prerouting rule that consults the map.
pub fn map_rule() -> Vec<String> {
    [
        "add",
        "rule",
        "ip",
        TABLE,
        "prerouting",
        "dnat",
        "ip",
        "addr",
        ".",
        "port",
        "to",
        "tcp",
        "dport",
        "map",
        &format!("@{MAP}"),
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Argv adding one element to the map.
pub fn element_add(entry: &PortMapEntry) -> Vec<String> {
    [
        "add",
        "element",
        "ip",
        TABLE,
        MAP,
        &format!(
            "{{ {} : {} . {} }}",
            entry.host_port, entry.container_ip, entry.container_port
        ),
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Argv deleting one element from the map, by host port (the key).
pub fn element_delete(host_port: u16) -> Vec<String> {
    [
        "delete",
        "element",
        "ip",
        TABLE,
        MAP,
        &format!("{{ {host_port} }}"),
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Extract the handles of legacy per-port DNAT rules from an
/// `nft -a list chain` listing.
///
/// The pre-map scheme added one `tcp dport N dnat to IP:PORT` rule per
/// container. On upgrade those linger ahead of the map rule and win,
/// so the one-time sweep deletes them by handle. The map's own lookup
/// rule (`... map @portmap`) prints as `dnat ip addr . port to ...`,
/// which never contains the literal `dnat to`, and map *elements*
/// don't appear in chain listings at all — neither can match here.
pub fn legacy_rule_handles(listing: &str) -> Vec<u64> {
    listing
        .lines()
        .filter(|line| {
            line.contains("dnat to") && line.contains("dport") && !line.contains("@portmap")
        })
        .filter_map(|line| line.rsplit("# handle ").next()?.trim().parse().ok())
        .collect()
}

/// Tracks which host ports are currently mapped, so repeated applies
/// are incremental and a mid-batch failure can be rolled back.
#[derive(Debug, Default)]
pub struct PortMapSet {
    applied: HashSet<u16>,
}

impl PortMapSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a host port is currently mapped.
    pub fn contains(&self, host_port: u16) -> bool {
        self.applied.contains(&host_port)
    }

    /// Add every entry not already applied. On a mid-batch failure the
    /// elements added by *this call* are deleted again (best-effort)
    /// and the first error is returned; previously applied entries are
    /// left untouched.
    pub async fn apply<E: NftExecutor>(
        &mut self,
        executor: &E,
        entries: &[PortMapEntry],
    ) -> Result<(), PortMapError> {
        let mut added_this_call: Vec<u16> = Vec::new();

        for entry in entries {
            if self.applied.contains(&entry.host_port) {
                continue;
            }
            if let Err(reason) = executor.run(&element_add(entry)).await {
                // Roll back what this call added before reporting.
                for port in &added_this_call {
                    let _ = executor.run(&element_delete(*port)).await;
                    self.applied.remove(port);
                }
                return Err(PortMapError::ApplyFailed {
                    host_port: entry.host_port,
                    container_port: entry.container_port,
                    reason,
                });
            }
            self.applied.insert(entry.host_port);
            added_this_call.push(entry.host_port);
        }

        Ok(())
    }

    /// Remove one mapping by host port. Removing an unknown port is a
    /// no-op (teardown paths run on best-effort cleanup).
    pub async fn remove<E: NftExecutor>(
        &mut self,
        executor: &E,
        host_port: u16,
    ) -> Result<(), PortMapError> {
        if !self.applied.remove(&host_port) {
            return Ok(());
        }
        executor
            .run(&element_delete(host_port))
            .await
            .map_err(|reason| PortMapError::RemoveFailed { host_port, reason })
    }
}

/// Production executor: shells out to the `nft` binary.
pub struct NftCommandExecutor;

impl NftExecutor for NftCommandExecutor {
    async fn run(&self, args: &[String]) -> Result<(), String> {
        let output = tokio::process::Command::new("nft")
            .args(args)
            .output()
            .await
            .map_err(|e| format!("failed to execute nft: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("nft {} failed: {stderr}", args.join(" ")));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Records every argv it receives; fails the nth call (0-based)
    /// when `fail_on_call` is set.
    pub(crate) struct RecordingExecutor {
        pub calls: tokio::sync::Mutex<Vec<Vec<String>>>,
        pub fail_on_call: Option<usize>,
    }

    impl RecordingExecutor {
        pub fn new() -> Self {
            Self {
                calls: tokio::sync::Mutex::new(Vec::new()),
                fail_on_call: None,
            }
        }

        pub fn failing_on(call_index: usize) -> Self {
            Self {
                calls: tokio::sync::Mutex::new(Vec::new()),
                fail_on_call: Some(call_index),
            }
        }

        pub async fn joined_calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .await
                .iter()
                .map(|args| args.join(" "))
                .collect()
        }
    }

    impl NftExecutor for RecordingExecutor {
        async fn run(&self, args: &[String]) -> Result<(), String> {
            let mut calls = self.calls.lock().await;
            let index = calls.len();
            calls.push(args.to_vec());
            if Some(index) == self.fail_on_call {
                return Err("scripted failure".to_string());
            }
            Ok(())
        }
    }

    fn entry(host_port: u16) -> PortMapEntry {
        PortMapEntry {
            host_port,
            container_ip: Ipv4Addr::new(10, 0, 2, 2),
            container_port: 8080,
        }
    }

    // -- argv generation -------------------------------------------------

    #[test]
    fn portmap_definition_syntax() {
        assert_eq!(
            portmap_definition().join(" "),
            "add map ip reliaburger portmap { type inet_service : ipv4_addr . inet_service ; }"
        );
    }

    #[test]
    fn map_rule_syntax() {
        assert_eq!(
            map_rule().join(" "),
            "add rule ip reliaburger prerouting dnat ip addr . port to tcp dport map @portmap"
        );
    }

    #[test]
    fn element_add_syntax() {
        assert_eq!(
            element_add(&entry(30001)).join(" "),
            "add element ip reliaburger portmap { 30001 : 10.0.2.2 . 8080 }"
        );
    }

    #[test]
    fn element_delete_syntax() {
        assert_eq!(
            element_delete(30001).join(" "),
            "delete element ip reliaburger portmap { 30001 }"
        );
    }

    #[test]
    fn braces_stay_one_argv_element() {
        // nft joins argv itself; the { ... } block must arrive as a
        // single token or nft parses it as separate arguments.
        let args = element_add(&entry(30001));
        assert_eq!(args.last().unwrap(), "{ 30001 : 10.0.2.2 . 8080 }");
        assert_eq!(element_delete(30001).last().unwrap(), "{ 30001 }");
    }

    // -- legacy sweep parsing ----------------------------------------------

    const CHAIN_LISTING: &str = "\
table ip reliaburger {
\tchain prerouting { # handle 1
\t\ttype nat hook prerouting priority dstnat; policy accept;
\t\ttcp dport 30001 dnat to 10.0.2.2:8080 # handle 42
\t\tdnat ip addr . port to tcp dport map @portmap # handle 44
\t\ttcp dport 30002 dnat to 10.0.2.3:9090 # handle 57
\t}
}";

    #[test]
    fn legacy_handles_found_for_per_port_rules() {
        assert_eq!(legacy_rule_handles(CHAIN_LISTING), vec![42, 57]);
    }

    #[test]
    fn legacy_handles_skip_the_map_rule() {
        let listing = "\tdnat ip addr . port to tcp dport map @portmap # handle 44\n";
        assert!(legacy_rule_handles(listing).is_empty());
    }

    #[test]
    fn legacy_handles_empty_chain() {
        assert!(legacy_rule_handles("table ip reliaburger {\n}\n").is_empty());
    }

    #[test]
    fn legacy_handles_ignore_lines_without_handles() {
        // A matching rule line with no trailing handle comment parses
        // to nothing rather than panicking or inventing a handle.
        let listing = "tcp dport 30001 dnat to 10.0.2.2:8080\n";
        assert!(legacy_rule_handles(listing).is_empty());
    }

    // -- apply/rollback ---------------------------------------------------

    #[tokio::test]
    async fn apply_rolls_back_on_mid_batch_failure() {
        // Second element add (call index 1) fails: the first must be
        // deleted again and the error surfaced.
        let executor = RecordingExecutor::failing_on(1);
        let mut set = PortMapSet::new();

        let result = set
            .apply(&executor, &[entry(30001), entry(30002), entry(30003)])
            .await;

        assert!(matches!(
            result,
            Err(PortMapError::ApplyFailed {
                host_port: 30002,
                ..
            })
        ));

        let calls = executor.joined_calls().await;
        assert_eq!(
            calls,
            vec![
                "add element ip reliaburger portmap { 30001 : 10.0.2.2 . 8080 }",
                "add element ip reliaburger portmap { 30002 : 10.0.2.2 . 8080 }",
                "delete element ip reliaburger portmap { 30001 }",
            ]
        );
        assert!(!set.contains(30001));
        assert!(!set.contains(30002));
    }

    #[tokio::test]
    async fn apply_is_incremental() {
        let executor = RecordingExecutor::new();
        let mut set = PortMapSet::new();

        set.apply(&executor, &[entry(30001), entry(30002)])
            .await
            .unwrap();
        // Second batch overlaps: only the new port is added.
        set.apply(&executor, &[entry(30001), entry(30002), entry(30003)])
            .await
            .unwrap();

        let calls = executor.joined_calls().await;
        assert_eq!(calls.len(), 3);
        assert!(calls[2].contains("{ 30003 :"));
        assert!(set.contains(30001) && set.contains(30002) && set.contains(30003));
    }

    #[tokio::test]
    async fn remove_deletes_by_host_port_only() {
        let executor = RecordingExecutor::new();
        let mut set = PortMapSet::new();

        set.apply(&executor, &[entry(30001)]).await.unwrap();
        set.remove(&executor, 30001).await.unwrap();

        let calls = executor.joined_calls().await;
        assert_eq!(calls[1], "delete element ip reliaburger portmap { 30001 }");
        assert!(!set.contains(30001));
    }

    #[tokio::test]
    async fn remove_unknown_port_is_a_noop() {
        let executor = RecordingExecutor::new();
        let mut set = PortMapSet::new();

        set.remove(&executor, 30999).await.unwrap();

        assert!(executor.joined_calls().await.is_empty());
    }
}
