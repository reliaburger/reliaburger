//! Explicit operator retirement after workloads have been stopped or fenced.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bound permanent identity tombstones without ever silently forgetting one.
pub const MAX_RETIRED_NODES: usize = 65_536;

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
}

/// Accept bounded, printable identities without interpreting them as URL paths.
pub fn validate_node_id(node_id: &str) -> Result<(), &'static str> {
    if node_id.is_empty() || node_id.len() > 256 || node_id.chars().any(char::is_control) {
        return Err("node identity must contain 1 to 256 printable bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
