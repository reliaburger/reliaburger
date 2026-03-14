/// SWIM node state machine.
///
/// Each node in the cluster is in one of four states: `Alive`, `Suspect`,
/// `Dead`, or `Left`. Transitions follow the SWIM protocol rules, with
/// incarnation numbers used for conflict resolution.
use std::fmt;

use serde::{Deserialize, Serialize};

/// The state of a node in the cluster membership.
///
/// State transitions:
/// ```text
/// Alive ──> Suspect ──> Dead
///   ▲          │
///   └──────────┘  (refutation with higher incarnation)
///
/// Any ──> Left  (graceful departure)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeState {
    /// Node is responding to probes.
    Alive,
    /// Node failed to respond; waiting for refutation or suspicion timeout.
    Suspect,
    /// Node confirmed unreachable; will be removed from membership.
    Dead,
    /// Node departed gracefully.
    Left,
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeState::Alive => write!(f, "alive"),
            NodeState::Suspect => write!(f, "suspect"),
            NodeState::Dead => write!(f, "dead"),
            NodeState::Left => write!(f, "left"),
        }
    }
}

/// Priority for dissemination ordering.
///
/// Higher values are disseminated first. Failures propagate faster
/// than joins, matching SWIM's emphasis on quick failure detection.
impl NodeState {
    pub fn dissemination_priority(self) -> u8 {
        match self {
            NodeState::Dead | NodeState::Left => 3,
            NodeState::Suspect => 2,
            NodeState::Alive => 1,
        }
    }

    /// Returns `true` if this state indicates the node is no longer
    /// participating in the cluster.
    pub fn is_down(self) -> bool {
        matches!(self, NodeState::Dead | NodeState::Left)
    }
}

/// Resolve a conflict between two updates about the same node.
///
/// Returns the winning `(state, incarnation)` pair. The rules follow
/// the SWIM protocol:
///
/// 1. Higher incarnation always wins.
/// 2. At equal incarnation, `Dead > Suspect > Alive`.
/// 3. `Left` always wins (explicit departure overrides everything).
pub fn resolve_conflict(
    old_state: NodeState,
    old_incarnation: u64,
    new_state: NodeState,
    new_incarnation: u64,
) -> (NodeState, u64) {
    // Left is terminal — graceful departure overrides everything
    if new_state == NodeState::Left {
        return (NodeState::Left, new_incarnation);
    }
    if old_state == NodeState::Left {
        return (NodeState::Left, old_incarnation);
    }

    if new_incarnation > old_incarnation {
        (new_state, new_incarnation)
    } else if new_incarnation < old_incarnation {
        (old_state, old_incarnation)
    } else {
        // Equal incarnation: more severe state wins
        if new_state.dissemination_priority() >= old_state.dissemination_priority() {
            (new_state, new_incarnation)
        } else {
            (old_state, old_incarnation)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn alive_to_suspect() {
        let (state, inc) = resolve_conflict(NodeState::Alive, 1, NodeState::Suspect, 1);
        assert_eq!(state, NodeState::Suspect);
        assert_eq!(inc, 1);
    }

    #[test]
    fn suspect_to_dead() {
        let (state, inc) = resolve_conflict(NodeState::Suspect, 1, NodeState::Dead, 1);
        assert_eq!(state, NodeState::Dead);
        assert_eq!(inc, 1);
    }

    #[test]
    fn suspect_refuted_by_higher_incarnation() {
        // Node was suspected, but it refutes by bumping its incarnation
        let (state, inc) = resolve_conflict(NodeState::Suspect, 1, NodeState::Alive, 2);
        assert_eq!(state, NodeState::Alive);
        assert_eq!(inc, 2);
    }

    #[test]
    fn higher_incarnation_always_wins() {
        let (state, inc) = resolve_conflict(NodeState::Dead, 1, NodeState::Alive, 5);
        assert_eq!(state, NodeState::Alive);
        assert_eq!(inc, 5);
    }

    #[test]
    fn lower_incarnation_loses() {
        let (state, inc) = resolve_conflict(NodeState::Alive, 5, NodeState::Dead, 1);
        assert_eq!(state, NodeState::Alive);
        assert_eq!(inc, 5);
    }

    #[test]
    fn equal_incarnation_suspect_beats_alive() {
        let (state, _) = resolve_conflict(NodeState::Alive, 3, NodeState::Suspect, 3);
        assert_eq!(state, NodeState::Suspect);
    }

    #[test]
    fn equal_incarnation_dead_beats_suspect() {
        let (state, _) = resolve_conflict(NodeState::Suspect, 3, NodeState::Dead, 3);
        assert_eq!(state, NodeState::Dead);
    }

    #[test]
    fn equal_incarnation_alive_does_not_beat_suspect() {
        let (state, _) = resolve_conflict(NodeState::Suspect, 3, NodeState::Alive, 3);
        assert_eq!(state, NodeState::Suspect);
    }

    #[test]
    fn left_always_wins_over_alive() {
        let (state, _) = resolve_conflict(NodeState::Alive, 100, NodeState::Left, 1);
        assert_eq!(state, NodeState::Left);
    }

    #[test]
    fn left_is_sticky() {
        // Once a node has left, even a higher incarnation alive doesn't revive it
        let (state, _) = resolve_conflict(NodeState::Left, 1, NodeState::Alive, 100);
        assert_eq!(state, NodeState::Left);
    }

    #[test]
    fn dead_and_left_are_down() {
        assert!(NodeState::Dead.is_down());
        assert!(NodeState::Left.is_down());
        assert!(!NodeState::Alive.is_down());
        assert!(!NodeState::Suspect.is_down());
    }

    #[test]
    fn dissemination_priority_ordering() {
        assert!(
            NodeState::Dead.dissemination_priority() > NodeState::Suspect.dissemination_priority()
        );
        assert!(
            NodeState::Suspect.dissemination_priority() > NodeState::Alive.dissemination_priority()
        );
        assert_eq!(
            NodeState::Dead.dissemination_priority(),
            NodeState::Left.dissemination_priority()
        );
    }

    #[test]
    fn display() {
        assert_eq!(NodeState::Alive.to_string(), "alive");
        assert_eq!(NodeState::Suspect.to_string(), "suspect");
        assert_eq!(NodeState::Dead.to_string(), "dead");
        assert_eq!(NodeState::Left.to_string(), "left");
    }

    #[test]
    fn serialisation_round_trip() {
        for state in [
            NodeState::Alive,
            NodeState::Suspect,
            NodeState::Dead,
            NodeState::Left,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: NodeState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, decoded);
        }
    }

    fn arb_node_state() -> impl Strategy<Value = NodeState> {
        prop_oneof![
            Just(NodeState::Alive),
            Just(NodeState::Suspect),
            Just(NodeState::Dead),
            Just(NodeState::Left),
        ]
    }

    proptest! {
        #[test]
        fn resolve_conflict_is_deterministic(
            old_state in arb_node_state(),
            old_inc in 0u64..100,
            new_state in arb_node_state(),
            new_inc in 0u64..100,
        ) {
            let (r1, i1) = resolve_conflict(old_state, old_inc, new_state, new_inc);
            let (r2, i2) = resolve_conflict(old_state, old_inc, new_state, new_inc);

            // Deterministic: same inputs always produce same output
            prop_assert_eq!(r1, r2);
            prop_assert_eq!(i1, i2);

            // Winner incarnation is one of the two inputs
            prop_assert!(i1 == old_inc || i1 == new_inc);

            // Left is terminal: if either side is Left, result is Left
            if old_state == NodeState::Left || new_state == NodeState::Left {
                prop_assert_eq!(r1, NodeState::Left);
            }

            // Higher incarnation wins (when neither is Left)
            if old_state != NodeState::Left && new_state != NodeState::Left {
                if new_inc > old_inc {
                    prop_assert_eq!(r1, new_state);
                    prop_assert_eq!(i1, new_inc);
                } else if new_inc < old_inc {
                    prop_assert_eq!(r1, old_state);
                    prop_assert_eq!(i1, old_inc);
                }
            }
        }
    }
}
