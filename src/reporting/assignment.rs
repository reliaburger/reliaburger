/// Parent assignment for the reporting tree.
///
/// Each worker node is assigned to exactly one council member as its
/// reporting parent. The assignment is deterministic: given the same
/// worker ID and council member list, every node in the cluster computes
/// the same parent. This avoids coordination — workers and council
/// members independently agree on the mapping.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::meat::NodeId;

/// Assign a worker to its parent council member.
///
/// Returns `None` if the council is empty. The council list is sorted
/// internally so the result is independent of input order.
///
/// Uses `DefaultHasher` (SipHash) which is deterministic within the
/// same binary. Since all nodes run the same Reliaburger binary, this
/// is safe for cross-node agreement.
pub fn assign_parent(worker_id: &NodeId, council_members: &[NodeId]) -> Option<NodeId> {
    if council_members.is_empty() {
        return None;
    }

    let mut sorted: Vec<&NodeId> = council_members.iter().collect();
    sorted.sort();

    let mut hasher = DefaultHasher::new();
    worker_id.hash(&mut hasher);
    let hash = hasher.finish();
    let index = (hash as usize) % sorted.len();

    Some(sorted[index].clone())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    #[test]
    fn empty_council_returns_none() {
        assert!(assign_parent(&node("w1"), &[]).is_none());
    }

    #[test]
    fn single_council_always_returns_same() {
        let council = vec![node("c1")];
        for i in 0..20 {
            let worker = node(&format!("worker-{i}"));
            assert_eq!(
                assign_parent(&worker, &council).unwrap(),
                node("c1"),
                "worker-{i} should map to the only council member"
            );
        }
    }

    #[test]
    fn distributes_across_council() {
        let council: Vec<NodeId> = (0..5).map(|i| node(&format!("council-{i}"))).collect();
        let mut counts: HashMap<NodeId, usize> = HashMap::new();

        for i in 0..100 {
            let worker = node(&format!("worker-{i}"));
            let parent = assign_parent(&worker, &council).unwrap();
            *counts.entry(parent).or_default() += 1;
        }

        // Every council member should get at least one worker.
        // With 100 workers and 5 members, the expected count is 20 each.
        for (member, count) in &counts {
            assert!(
                *count > 0,
                "council member {member:?} got zero workers assigned"
            );
        }
        assert_eq!(counts.len(), 5, "all 5 council members should appear");
    }

    #[test]
    fn deterministic_same_inputs_same_output() {
        let council = vec![node("c1"), node("c2"), node("c3")];
        let worker = node("worker-42");

        let first = assign_parent(&worker, &council).unwrap();
        let second = assign_parent(&worker, &council).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn changes_when_council_changes() {
        let council_full = vec![node("c1"), node("c2"), node("c3"), node("c4"), node("c5")];
        let council_reduced = vec![node("c1"), node("c2"), node("c3")];

        // At least one of 50 workers should remap when the council shrinks
        let mut any_changed = false;
        for i in 0..50 {
            let worker = node(&format!("w-{i}"));
            let parent_full = assign_parent(&worker, &council_full).unwrap();
            let parent_reduced = assign_parent(&worker, &council_reduced).unwrap();
            if parent_full != parent_reduced {
                any_changed = true;
                break;
            }
        }
        assert!(
            any_changed,
            "some workers should remap when council shrinks"
        );
    }

    #[test]
    fn independent_of_input_order() {
        let council_a = vec![node("c3"), node("c1"), node("c2")];
        let council_b = vec![node("c1"), node("c2"), node("c3")];
        let council_c = vec![node("c2"), node("c3"), node("c1")];

        let worker = node("worker-7");
        let parent_a = assign_parent(&worker, &council_a).unwrap();
        let parent_b = assign_parent(&worker, &council_b).unwrap();
        let parent_c = assign_parent(&worker, &council_c).unwrap();

        assert_eq!(parent_a, parent_b);
        assert_eq!(parent_b, parent_c);
    }
}
