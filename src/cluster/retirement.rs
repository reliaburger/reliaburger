//! Explicit operator retirement after workloads have been stopped or fenced.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bound permanent identity tombstones without ever silently forgetting one.
/// At the bound, every further decommission is refused with "retired
/// identity limit reached"; nothing removes a tombstone, so `relish council
/// status` and `relish wtf` report how full the list is (#2375).
pub const MAX_RETIRED_NODES: usize = 65_536;

/// How full the retired-identity list may get, in percent of
/// [`MAX_RETIRED_NODES`], before `relish wtf` and `relish council status`
/// warn.
pub const RETIRED_IDENTITY_WARNING_PERCENT: usize = 90;

/// How many permanent identity tombstones the cluster holds, against the
/// limit (#2375).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredIdentities {
    /// Retired identities recorded in the replicated CRL.
    pub count: usize,
    /// The most there can be: [`MAX_RETIRED_NODES`].
    pub limit: usize,
}

impl RetiredIdentities {
    /// The usage for `count` tombstones against [`MAX_RETIRED_NODES`].
    pub fn of(count: usize) -> Self {
        Self {
            count,
            limit: MAX_RETIRED_NODES,
        }
    }

    /// Whether the list is at or past [`RETIRED_IDENTITY_WARNING_PERCENT`]
    /// of its limit.
    pub fn nearly_full(&self) -> bool {
        self.count.saturating_mul(100)
            >= self.limit.saturating_mul(RETIRED_IDENTITY_WARNING_PERCENT)
    }
}

/// The operator's attestation that this node can no longer run its old workloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecommissionRequest {
    /// Cluster identity being retired permanently.
    pub node_id: String,
    /// Explicit confirmation of external shutdown or fencing.
    pub workloads_stopped: bool,
    /// Human-readable maintenance or replacement reason.
    pub reason: String,
}

impl DecommissionRequest {
    /// Validate the attestation before contacting or mutating a cluster.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_node_id(&self.node_id)?;
        if !self.workloads_stopped {
            return Err(
                "decommissioning requires confirmation that workloads are stopped or fenced",
            );
        }
        if self.reason.trim().is_empty() || self.reason.len() > 1024 {
            return Err("decommission reason must contain 1 to 1024 bytes");
        }
        Ok(())
    }
}

/// The immutable result of an operator's decommissioning decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRetirement {
    /// Permanently retired node identity; re-enrolment must use a new name.
    pub node_id: String,
    /// Authenticated operator credential identity, supplied by the server.
    pub retired_by: String,
    /// Operator's explanation of the shutdown or fencing.
    pub reason: String,
    /// Time of the original decision, committed rather than read on replicas.
    pub retired_at_unix_ms: u64,
    /// Number of unresolved placement obligations released for each lease.
    pub released_placements: BTreeMap<String, u64>,
    /// Number of unresolved repository writer obligations released for each lease.
    #[serde(default)]
    pub released_registry_writers: BTreeMap<String, u64>,
    /// Node-chaos obligation resolved by the same external fencing attestation.
    #[serde(default)]
    pub released_node_fault: Option<u64>,
    /// This fenced identity no longer retains cluster discovery publications.
    #[serde(default)]
    pub released_endpoint_consumer: bool,
}

/// Accept only identities a node could have started with: one DNS label,
/// the rule `[node] name` follows (#2394).
pub fn validate_node_id(node_id: &str) -> Result<(), &'static str> {
    if !crate::config::node::is_valid_node_name(node_id) {
        return Err(
            "node identity must be one DNS label: 1 to 63 lowercase letters, digits and '-'",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retired_identity_list_is_nearly_full_from_ninety_percent() {
        let threshold = (MAX_RETIRED_NODES * RETIRED_IDENTITY_WARNING_PERCENT).div_ceil(100);
        assert_eq!(threshold, 58_983);
        assert!(!RetiredIdentities::of(0).nearly_full());
        assert!(!RetiredIdentities::of(threshold - 1).nearly_full());
        assert!(RetiredIdentities::of(threshold).nearly_full());
        assert!(RetiredIdentities::of(MAX_RETIRED_NODES).nearly_full());
    }

    #[test]
    fn decommission_requires_an_identity_reason_and_explicit_attestation() {
        let good = DecommissionRequest {
            node_id: "worker-01".into(),
            workloads_stopped: true,
            reason: "powered off".into(),
        };
        assert!(good.validate().is_ok());
        for bad in [
            DecommissionRequest {
                workloads_stopped: false,
                ..good.clone()
            },
            DecommissionRequest {
                reason: " ".into(),
                ..good.clone()
            },
            DecommissionRequest {
                reason: "x".repeat(1025),
                ..good.clone()
            },
            DecommissionRequest {
                node_id: String::new(),
                ..good.clone()
            },
            DecommissionRequest {
                node_id: "worker\nother".into(),
                ..good.clone()
            },
            DecommissionRequest {
                node_id: "a".repeat(64),
                ..good.clone()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
