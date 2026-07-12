/// Parent assignment for the reporting tree.
///
/// Each worker node is assigned to exactly one council member as its
/// reporting parent. The assignment is deterministic: given the same
/// worker ID and council member list, every node in the cluster computes
/// the same parent. This avoids coordination — workers and council
/// members independently agree on the mapping.
use crate::meat::NodeId;

/// FNV-1a offset basis (64-bit).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a prime (64-bit).
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 64-bit FNV-1a over a byte string.
///
/// The parent assignment used to hash with
/// `std::collections::hash_map::DefaultHasher`, whose algorithm the
/// standard library explicitly does NOT guarantee across Rust versions —
/// two nodes built with different toolchains could disagree about the
/// reporting tree (CP10). FNV-1a is a fixed, published algorithm: the
/// mapping is pinned by tests and can never drift with a compiler
/// upgrade. (The 32-bit variant already lives in `smoker::bpf_types` for
/// eBPF map keys; reporting needs 64 bits, kept local to avoid a
/// cross-subsystem dependency.)
fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Assign a worker to its parent council member.
///
/// Returns `None` if the council is empty. The council list is sorted
/// internally so the result is independent of input order, and the hash
/// is version-stable (FNV-1a), so every node — whatever toolchain built
/// its binary — computes the same parent.
pub fn assign_parent(worker_id: &NodeId, council_members: &[NodeId]) -> Option<NodeId> {
    if council_members.is_empty() {
        return None;
    }

    let mut sorted: Vec<&NodeId> = council_members.iter().collect();
    sorted.sort();

    let hash = fnv1a_64(worker_id.0.as_bytes());
    let index = (hash % sorted.len() as u64) as usize;

    Some(sorted[index].clone())
}

/// Assign a worker to its parent council member and return the address.
///
/// Convenience wrapper around `assign_parent` that also resolves the
/// parent's `SocketAddr` from a `(NodeId, SocketAddr)` list.
pub fn assign_parent_address(
    worker_id: &NodeId,
    council: &[(NodeId, std::net::SocketAddr)],
) -> Option<std::net::SocketAddr> {
    let council_ids: Vec<NodeId> = council.iter().map(|(id, _)| id.clone()).collect();
    let parent_id = assign_parent(worker_id, &council_ids)?;
    council
        .iter()
        .find(|(id, _)| *id == parent_id)
        .map(|(_, addr)| *addr)
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

    /// Snapshot-style pin: these exact assignments must NEVER change.
    ///
    /// Every node computes the tree independently; if a toolchain or
    /// refactor altered the hash, workers and aggregators would silently
    /// disagree about who reports where. Expected values were computed
    /// from the published FNV-1a algorithm (offset basis
    /// 0xcbf29ce484222325, prime 0x100000001b3).
    #[test]
    fn assignment_is_pinned_across_versions() {
        let council: Vec<NodeId> = (1..=5).map(|i| node(&format!("c{i}"))).collect();
        let expected = [
            ("worker-0", "c5"),
            ("worker-1", "c1"),
            ("worker-2", "c3"),
            ("worker-42", "c2"),
            ("node-a", "c2"),
            ("node-b", "c3"),
            ("a-very-long-node-name-0123456789", "c1"),
        ];
        for (worker, parent) in expected {
            assert_eq!(
                assign_parent(&node(worker), &council).unwrap(),
                node(parent),
                "pinned parent for {worker} drifted"
            );
        }

        let small_council = vec![node("c1"), node("c2"), node("c3")];
        for (worker, parent) in [("worker-0", "c3"), ("worker-42", "c3"), ("node-b", "c2")] {
            assert_eq!(
                assign_parent(&node(worker), &small_council).unwrap(),
                node(parent),
                "pinned parent for {worker} (3-member council) drifted"
            );
        }
    }

    #[test]
    fn fnv1a_matches_published_test_vectors() {
        // From the FNV reference implementation.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
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
