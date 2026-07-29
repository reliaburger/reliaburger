//! Reversible node-level fault state shared with cluster transports.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared switch which makes every cluster transport behave as if this node
/// had disappeared.
///
/// Faults are reference counted. If two independently owned experiments
/// quiesce the node, clearing either one must not heal the other.
#[derive(Clone, Default)]
pub struct NodeTransportGate {
    active_faults: Arc<AtomicUsize>,
}

impl NodeTransportGate {
    /// Create an open transport gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Quiesce cluster traffic and retain one fault owner's reference.
    pub fn quiesce(&self) {
        self.active_faults.fetch_add(1, Ordering::AcqRel);
    }

    /// Release one fault owner's reference.
    ///
    /// An unmatched restore is harmless. This matters during best-effort
    /// cleanup, where an already-reversed fault may be visited again.
    pub fn restore(&self) {
        let _ = self
            .active_faults
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            });
    }

    /// Whether cluster transports must currently drop all traffic.
    pub fn is_quiesced(&self) -> bool {
        self.active_faults.load(Ordering::Acquire) > 0
    }
}

#[cfg(test)]
mod tests {
    use super::NodeTransportGate;

    #[test]
    fn transport_quiesce_is_reference_counted() {
        let gate = NodeTransportGate::new();
        assert!(!gate.is_quiesced());

        gate.quiesce();
        gate.quiesce();
        assert!(gate.is_quiesced());

        gate.restore();
        assert!(
            gate.is_quiesced(),
            "one fault must not restore another fault's transport outage"
        );
        gate.restore();
        assert!(!gate.is_quiesced());

        gate.restore();
        assert!(!gate.is_quiesced(), "an extra restore must not underflow");
    }
}
