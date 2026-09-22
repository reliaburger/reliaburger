//! Durable admission for node chaos. Deadlines trigger fencing, never release.

use serde::{Deserialize, Serialize};

/// One cluster-wide node fault awaiting activation or confirmed reversal.
///
/// Serialising node experiments is deliberately conservative: even a pressure
/// experiment can remove a voter from service. The boot identity and increasing
/// sequence let the target reject an activation that arrives after cleanup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeFaultReservation {
    /// Monotonically increasing cluster allocation number.
    pub sequence: u64,
    /// Target agent incarnation, generated afresh on process start.
    pub boot_id: String,
    /// Exact normalised operation admitted by the leader.
    pub request: super::types::FaultRequest,
    /// Wall-clock cleanup trigger. Expiry alone does not free capacity.
    pub cleanup_after_unix_ms: u64,
}

/// Replicated ownership; a new leader inherits the outstanding reservation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeFaultReservations {
    /// Last allocated number, retained after cleanup to prevent reuse.
    pub last_sequence: u64,
    /// At most one node experiment may own cluster capacity.
    pub active: Option<NodeFaultReservation>,
}

impl NodeFaultReservations {
    /// Atomically reserve capacity after the state machine checks membership.
    pub fn reserve(&mut self, reservation: &NodeFaultReservation) -> Result<(), String> {
        if let Some(active) = &self.active {
            return if active == reservation {
                Ok(())
            } else {
                Err(format!(
                    "node fault capacity is reserved by operation {}",
                    active.sequence
                ))
            };
        }
        if self.last_sequence.checked_add(1) != Some(reservation.sequence) {
            return Err("stale or exhausted node fault reservation sequence".to_string());
        }
        if reservation.boot_id.is_empty()
            || reservation
                .request
                .target_node
                .as_deref()
                .is_none_or(str::is_empty)
            || reservation.request.duration.is_zero()
            || reservation.cleanup_after_unix_ms == 0
            || !reservation.request.fault_type.requires_admin_reversal()
        {
            return Err("invalid node fault reservation".to_string());
        }
        self.last_sequence = reservation.sequence;
        self.active = Some(reservation.clone());
        Ok(())
    }

    /// Free capacity only after the target acknowledged a fence and reversal.
    pub fn release(&mut self, sequence: u64) -> Result<(), String> {
        match &self.active {
            Some(active) if active.sequence == sequence => {
                self.active = None;
                Ok(())
            }
            None if sequence == self.last_sequence && sequence != 0 => Ok(()),
            _ => Err("node fault release does not match the current reservation".to_string()),
        }
    }
}

/// Target-side fence. A new process has a new boot identity; within a process,
/// both activation and reversal consume the monotonically increasing sequence.
#[derive(Debug)]
pub struct NodeFaultFence {
    /// Identity of this agent process, never accepted from an HTTP caller.
    pub boot_id: String,
    through: u64,
    /// Effect still owned locally, including a failed cleanup awaiting retry.
    pub active: Option<(u64, super::types::FaultId)>,
}

impl Default for NodeFaultFence {
    fn default() -> Self {
        use ring::rand::SecureRandom;
        let mut bytes = [0; 32];
        // An unavailable OS entropy source disables admission. Never fall back
        // to a timestamp or another identity that a later boot could repeat.
        let boot_id = if ring::rand::SystemRandom::new().fill(&mut bytes).is_ok() {
            hex::encode(bytes)
        } else {
            String::new()
        };
        Self {
            boot_id,
            through: 0,
            active: None,
        }
    }
}

impl NodeFaultFence {
    /// Consume an exact grant before beginning any externally visible effect.
    pub fn activate(
        &mut self,
        grant: &NodeFaultReservation,
        request: &super::types::FaultRequest,
    ) -> Result<(), String> {
        if self.boot_id.is_empty()
            || grant.boot_id != self.boot_id
            || grant.sequence <= self.through
            || &grant.request != request
            || self.active.is_some()
        {
            return Err("node fault grant is stale, mismatched or already consumed".into());
        }
        self.through = grant.sequence;
        Ok(())
    }

    /// Whether activation has already been consumed, or belongs to an old boot.
    pub fn consumed(&self, grant: &NodeFaultReservation) -> bool {
        grant.boot_id != self.boot_id || grant.sequence <= self.through
    }

    /// Fence late activation before reversing the associated effect. A boot
    /// mismatch already prevents activation; the caller must still establish
    /// that any pressure helpers from the old process have been recovered.
    pub fn fence(&mut self, grant: &NodeFaultReservation) -> Option<super::types::FaultId> {
        if grant.boot_id != self.boot_id {
            return None;
        }
        self.through = self.through.max(grant.sequence);
        self.active
            .filter(|(sequence, _)| *sequence <= grant.sequence)
            .map(|(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(sequence: u64, node: &str) -> NodeFaultReservation {
        NodeFaultReservation {
            sequence,
            boot_id: "boot-a".into(),
            cleanup_after_unix_ms: 100,
            request: super::super::types::FaultRequest {
                fault_type: super::super::types::FaultType::NodeKill {
                    kill_containers: false,
                },
                target_service: String::new(),
                namespace: None,
                target_instance: None,
                target_node: Some(node.into()),
                duration: std::time::Duration::from_secs(10),
                injected_by: "operator".into(),
                reason: None,
                include_leader: true,
                override_safety: true,
                acknowledged: true,
            },
        }
    }

    #[test]
    fn late_duplicate_and_previous_boot_activation_are_refused() {
        let mut fence = NodeFaultFence::default();
        let mut grant = reservation(1, "node-a");
        grant.boot_id = fence.boot_id.clone();
        let mut altered = grant.request.clone();
        altered.duration *= 2;
        assert!(fence.activate(&grant, &altered).is_err());
        fence.fence(&grant);
        assert!(fence.activate(&grant, &grant.request).is_err());
        grant.sequence = 2;
        fence.activate(&grant, &grant.request).unwrap();
        assert!(fence.activate(&grant, &grant.request).is_err());
        assert!(
            NodeFaultFence::default()
                .activate(&grant, &grant.request)
                .is_err()
        );
        fence.active = Some((2, super::super::types::FaultId(42)));
        assert_eq!(fence.fence(&grant), Some(super::super::types::FaultId(42)));
        grant.sequence = 1;
        assert_eq!(fence.fence(&grant), None);
    }

    #[test]
    fn concurrent_requests_cannot_share_node_fault_capacity() {
        let mut ledger = NodeFaultReservations::default();
        let first = reservation(1, "node-a");
        ledger.reserve(&first).unwrap();
        assert!(ledger.reserve(&reservation(1, "node-b")).is_err());
        assert!(ledger.reserve(&reservation(2, "node-b")).is_err());
        assert_eq!(ledger.active, Some(first.clone()));
        // An identical retry may recover a lost admission response.
        ledger.reserve(&first).unwrap();
        // Even an elapsed deadline cannot silently release the reservation.
        let persisted = serde_json::to_vec(&ledger).unwrap();
        let mut successor: NodeFaultReservations = serde_json::from_slice(&persisted).unwrap();
        assert!(successor.reserve(&reservation(2, "node-b")).is_err());
        assert!(successor.release(2).is_err());
        successor.release(1).unwrap();
        assert!(successor.reserve(&first).is_err());
        successor.reserve(&reservation(2, "node-b")).unwrap();
        assert!(successor.release(1).is_err());
        assert_eq!(successor.active.as_ref().unwrap().sequence, 2);
    }

    #[test]
    fn invalid_or_exhausted_reservations_do_not_mutate_ownership() {
        let mut ledger = NodeFaultReservations::default();
        for invalid in [0, 2, u64::MAX] {
            assert!(ledger.reserve(&reservation(invalid, "node-a")).is_err());
            assert_eq!(ledger, NodeFaultReservations::default());
        }
        let mut invalid = reservation(1, "node-a");
        invalid.boot_id.clear();
        assert!(ledger.reserve(&invalid).is_err());
        let mut invalid = reservation(1, "node-a");
        invalid.request.target_node = None;
        assert!(ledger.reserve(&invalid).is_err());
        let mut invalid = reservation(1, "node-a");
        invalid.request.duration = std::time::Duration::ZERO;
        assert!(ledger.reserve(&invalid).is_err());
        ledger.last_sequence = u64::MAX;
        assert!(ledger.reserve(&reservation(0, "node-a")).is_err());
    }
}
