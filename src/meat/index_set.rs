//! Compact sets of task indices, stored as ranges.
//!
//! A task array with a million tasks needs to remember which chunks are
//! queued, which are done and which indices failed. Stored one entry per
//! index, that's megabytes of Raft state; stored as ranges, a million
//! finished tasks in order is a single `[0, 999999]` pair, and sparse
//! failures cost eight bytes each. [`IndexRangeSet`] is that representation.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

/// A set of `u32` indices kept as sorted, disjoint, non-adjacent inclusive
/// ranges.
///
/// The invariant (checked on deserialisation, maintained by every method):
/// each range has `first <= last`, and each range starts at least two past
/// the previous range's end, so touching ranges are always merged. That
/// makes the representation canonical: two equal sets serialise to the
/// same JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<[u32; 2]>", into = "Vec<[u32; 2]>")]
pub struct IndexRangeSet {
    ranges: Vec<(u32, u32)>,
}

/// Why a serialised range list isn't a valid [`IndexRangeSet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexSetError {
    #[error("range {position} is reversed: [{first}, {last}]")]
    Reversed {
        position: usize,
        first: u32,
        last: u32,
    },
    #[error("range {position} overlaps, touches or precedes the range before it")]
    Unordered { position: usize },
}

impl IndexRangeSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// A set holding every index in `range`.
    pub fn from_range(range: RangeInclusive<u32>) -> Self {
        let mut set = Self::new();
        set.insert_range(range);
        set
    }

    /// Whether the set has no members.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of members. A `u64`, because the full `u32` domain has
    /// 2^32 members, one more than `u32` can count.
    pub fn len(&self) -> u64 {
        self.ranges
            .iter()
            .map(|&(first, last)| u64::from(last) - u64::from(first) + 1)
            .sum()
    }

    /// Number of stored ranges: the set's real size in memory and on the
    /// wire.
    pub fn range_count(&self) -> usize {
        self.ranges.len()
    }

    /// Smallest member, if any.
    pub fn first(&self) -> Option<u32> {
        self.ranges.first().map(|&(first, _)| first)
    }

    /// Whether `index` is a member.
    pub fn contains(&self, index: u32) -> bool {
        let position = self.ranges.partition_point(|&(_, last)| last < index);
        self.ranges
            .get(position)
            .is_some_and(|&(first, _)| first <= index)
    }

    /// Whether every index in `range` is a member.
    pub fn contains_range(&self, range: RangeInclusive<u32>) -> bool {
        let (first, last) = (*range.start(), *range.end());
        if first > last {
            return true;
        }
        let position = self.ranges.partition_point(|&(_, end)| end < first);
        self.ranges
            .get(position)
            .is_some_and(|&(start, end)| start <= first && last <= end)
    }

    /// Add `index`. Returns whether it was newly added.
    pub fn insert(&mut self, index: u32) -> bool {
        if self.contains(index) {
            return false;
        }
        self.insert_range(index..=index);
        true
    }

    /// Add every index in `range`, merging with neighbours. An empty
    /// (reversed) range is ignored.
    pub fn insert_range(&mut self, range: RangeInclusive<u32>) {
        let (first, last) = (*range.start(), *range.end());
        if first > last {
            return;
        }
        // u64 arithmetic so `last + 1` can't overflow at u32::MAX.
        let start = self
            .ranges
            .partition_point(|&(_, end)| u64::from(end) + 1 < u64::from(first));
        let stop = self
            .ranges
            .partition_point(|&(begin, _)| u64::from(begin) <= u64::from(last) + 1);
        let merged = if start < stop {
            (
                first.min(self.ranges[start].0),
                last.max(self.ranges[stop - 1].1),
            )
        } else {
            (first, last)
        };
        self.ranges.splice(start..stop, [merged]);
    }

    /// Remove `index`. Returns whether it was a member.
    pub fn remove(&mut self, index: u32) -> bool {
        if !self.contains(index) {
            return false;
        }
        self.remove_range(index..=index);
        true
    }

    /// Remove every index in `range`, splitting ranges where needed. An
    /// empty (reversed) range is ignored.
    pub fn remove_range(&mut self, range: RangeInclusive<u32>) {
        let (first, last) = (*range.start(), *range.end());
        if first > last {
            return;
        }
        let start = self.ranges.partition_point(|&(_, end)| end < first);
        let stop = self.ranges.partition_point(|&(begin, _)| begin <= last);
        if start >= stop {
            return;
        }
        let mut pieces = Vec::with_capacity(2);
        let (head_first, _) = self.ranges[start];
        let (_, tail_last) = self.ranges[stop - 1];
        if head_first < first {
            pieces.push((head_first, first - 1));
        }
        if tail_last > last {
            pieces.push((last + 1, tail_last));
        }
        self.ranges.splice(start..stop, pieces);
    }

    /// Add every member of `other`.
    pub fn extend_from(&mut self, other: &IndexRangeSet) {
        for range in other.ranges() {
            self.insert_range(range);
        }
    }

    /// Remove and return the `count` smallest members (fewer if the set
    /// is smaller). This is how the leader hands out the lowest queued
    /// chunks first.
    pub fn take_first(&mut self, count: u64) -> IndexRangeSet {
        // Count the whole ranges first and drain them in one go, so taking
        // from a fragmented set stays linear rather than shifting the
        // vector once per range.
        let mut remaining = count;
        let mut whole = 0;
        for &(first, last) in &self.ranges {
            let width = u64::from(last) - u64::from(first) + 1;
            if width > remaining {
                break;
            }
            remaining -= width;
            whole += 1;
        }
        let mut taken = IndexRangeSet {
            ranges: self.ranges.drain(..whole).collect(),
        };
        if remaining > 0
            && let Some(head) = self.ranges.first_mut()
        {
            // remaining < width of this range <= 2^32, so the cut fits in a u32.
            let cut = head.0 + (remaining as u32) - 1;
            taken.ranges.push((head.0, cut));
            head.0 = cut + 1;
        }
        taken
    }

    /// The stored ranges, smallest first.
    pub fn ranges(&self) -> impl Iterator<Item = RangeInclusive<u32>> + '_ {
        self.ranges.iter().map(|&(first, last)| first..=last)
    }

    /// Every member, smallest first. Meant for small sets; a set holding a
    /// million indices yields a million items.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.ranges().flatten()
    }
}

impl TryFrom<Vec<[u32; 2]>> for IndexRangeSet {
    type Error = IndexSetError;

    fn try_from(pairs: Vec<[u32; 2]>) -> Result<Self, Self::Error> {
        let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(pairs.len());
        for (position, [first, last]) in pairs.into_iter().enumerate() {
            if first > last {
                return Err(IndexSetError::Reversed {
                    position,
                    first,
                    last,
                });
            }
            if let Some(&(_, previous_last)) = ranges.last()
                && u64::from(first) <= u64::from(previous_last) + 1
            {
                return Err(IndexSetError::Unordered { position });
            }
            ranges.push((first, last));
        }
        Ok(Self { ranges })
    }
}

impl From<IndexRangeSet> for Vec<[u32; 2]> {
    fn from(set: IndexRangeSet) -> Self {
        set.ranges
            .into_iter()
            .map(|(first, last)| [first, last])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    fn pairs(set: &IndexRangeSet) -> Vec<(u32, u32)> {
        set.ranges().map(|r| (*r.start(), *r.end())).collect()
    }

    #[test]
    fn empty_set_has_no_members() {
        let set = IndexRangeSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert_eq!(set.first(), None);
        assert!(!set.contains(0));
    }

    #[test]
    fn insert_merges_with_neighbours_on_both_sides() {
        let mut set = IndexRangeSet::new();
        assert!(set.insert(1));
        assert!(set.insert(3));
        assert_eq!(pairs(&set), vec![(1, 1), (3, 3)]);
        assert!(set.insert(2));
        assert_eq!(pairs(&set), vec![(1, 3)]);
    }

    #[test]
    fn inserting_an_existing_member_changes_nothing() {
        let mut set = IndexRangeSet::from_range(10..=20);
        assert!(!set.insert(15));
        assert_eq!(pairs(&set), vec![(10, 20)]);
    }

    #[test]
    fn insert_range_coalesces_several_ranges() {
        let mut set = IndexRangeSet::new();
        for i in [0, 4, 8, 12] {
            set.insert(i);
        }
        set.insert_range(3..=9);
        assert_eq!(pairs(&set), vec![(0, 0), (3, 9), (12, 12)]);
        set.insert_range(1..=2);
        assert_eq!(pairs(&set), vec![(0, 9), (12, 12)]);
    }

    #[test]
    fn remove_range_splits_a_range() {
        let mut set = IndexRangeSet::from_range(0..=99);
        set.remove_range(10..=19);
        assert_eq!(pairs(&set), vec![(0, 9), (20, 99)]);
        assert!(set.remove(0));
        assert!(!set.remove(0));
        assert_eq!(pairs(&set), vec![(1, 9), (20, 99)]);
        assert_eq!(set.len(), 89);
    }

    #[test]
    fn contains_is_exact_at_range_edges() {
        let set = IndexRangeSet::from_range(5..=7);
        assert!(!set.contains(4));
        assert!(set.contains(5));
        assert!(set.contains(7));
        assert!(!set.contains(8));
        assert!(set.contains_range(5..=7));
        assert!(!set.contains_range(4..=6));
    }

    #[test]
    fn full_u32_domain_counts_without_overflow() {
        let mut set = IndexRangeSet::from_range(0..=u32::MAX);
        assert_eq!(set.len(), 1u64 << 32);
        assert!(set.contains(u32::MAX));
        set.remove(u32::MAX);
        assert_eq!(pairs(&set), vec![(0, u32::MAX - 1)]);
        set.insert(u32::MAX);
        assert_eq!(set.range_count(), 1);
    }

    #[test]
    fn take_first_splits_the_lowest_members_off() {
        let mut set = IndexRangeSet::new();
        set.insert_range(0..=2);
        set.insert_range(10..=19);
        let taken = set.take_first(5);
        assert_eq!(pairs(&taken), vec![(0, 2), (10, 11)]);
        assert_eq!(pairs(&set), vec![(12, 19)]);
        let rest = set.take_first(100);
        assert_eq!(rest.len(), 8);
        assert!(set.is_empty());
    }

    #[test]
    fn serialises_as_pairs_of_first_and_last() {
        let mut set = IndexRangeSet::from_range(0..=999_999);
        set.remove(42);
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, "[[0,41],[43,999999]]");
        let back: IndexRangeSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set);
    }

    #[test]
    fn deserialise_refuses_non_canonical_ranges() {
        for bad in [
            "[[5,1]]",
            "[[0,5],[3,9]]",
            "[[0,5],[6,9]]",
            "[[10,12],[0,1]]",
        ] {
            assert!(
                serde_json::from_str::<IndexRangeSet>(bad).is_err(),
                "{bad} must be refused"
            );
        }
        assert!(serde_json::from_str::<IndexRangeSet>("[[0,5],[7,9]]").is_ok());
    }

    #[derive(Debug, Clone)]
    enum Operation {
        Insert(u32),
        InsertRange(u32, u32),
        Remove(u32),
        RemoveRange(u32, u32),
        TakeFirst(u64),
    }

    fn operation() -> impl Strategy<Value = Operation> {
        // A small domain makes merges and splits collide often.
        prop_oneof![
            (0u32..200).prop_map(Operation::Insert),
            (0u32..200, 0u32..30).prop_map(|(a, w)| Operation::InsertRange(a, a + w)),
            (0u32..200).prop_map(Operation::Remove),
            (0u32..200, 0u32..30).prop_map(|(a, w)| Operation::RemoveRange(a, a + w)),
            (0u64..20).prop_map(Operation::TakeFirst),
        ]
    }

    fn assert_canonical(set: &IndexRangeSet) {
        let stored = pairs(set);
        for (first, last) in &stored {
            assert!(first <= last);
        }
        for window in stored.windows(2) {
            assert!(u64::from(window[1].0) > u64::from(window[0].1) + 1);
        }
    }

    proptest! {
        #[test]
        fn behaves_like_a_btreeset(operations in proptest::collection::vec(operation(), 1..120)) {
            let mut set = IndexRangeSet::new();
            let mut model = BTreeSet::new();
            for op in operations {
                match op {
                    Operation::Insert(i) => {
                        prop_assert_eq!(set.insert(i), model.insert(i));
                    }
                    Operation::InsertRange(a, b) => {
                        set.insert_range(a..=b);
                        model.extend(a..=b);
                    }
                    Operation::Remove(i) => {
                        prop_assert_eq!(set.remove(i), model.remove(&i));
                    }
                    Operation::RemoveRange(a, b) => {
                        set.remove_range(a..=b);
                        for i in a..=b {
                            model.remove(&i);
                        }
                    }
                    Operation::TakeFirst(n) => {
                        let taken: Vec<u32> = set.take_first(n).iter().collect();
                        let expected: Vec<u32> = model.iter().copied().take(n as usize).collect();
                        for i in &expected {
                            model.remove(i);
                        }
                        prop_assert_eq!(taken, expected);
                    }
                }
                assert_canonical(&set);
                prop_assert_eq!(set.len(), model.len() as u64);
                prop_assert_eq!(set.iter().collect::<Vec<_>>(), model.iter().copied().collect::<Vec<_>>());
            }
            let json = serde_json::to_string(&set).unwrap();
            let back: IndexRangeSet = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, set);
        }
    }
}
