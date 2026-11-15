//! Counters the gossip layer keeps about itself, for `relish status`.
//!
//! The Mustard node owns its membership table outright (it's driven by
//! one task, with `&mut self`), so nothing else can read it in place. These
//! counters are the exception: atomics behind an `Arc`, which the gossip
//! task bumps and the API reads without a lock or a channel. A
//! `watch` channel would also work, but every received datagram would then
//! publish a new value to wake nobody; an atomic store is one instruction.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A shared handle on one node's gossip counters. Cloning it shares them.
#[derive(Debug, Clone, Default)]
pub struct GossipStats {
    inner: Arc<Counters>,
}

#[derive(Debug, Default)]
struct Counters {
    updates_received: AtomicU64,
    /// Unix milliseconds of the last membership change. Zero means none
    /// yet: the atomic can't hold an `Option`, and no real change happens
    /// at the epoch. [`GossipStats::last_change_unix_ms`] turns it back
    /// into an `Option`.
    last_change_unix_ms: AtomicU64,
}

impl GossipStats {
    /// Count `count` membership updates received in one message.
    pub fn record_updates(&self, count: usize) {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        // Relaxed: a counter read for display needs no ordering with any
        // other memory.
        self.inner
            .updates_received
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Stamp a membership change at `unix_ms`.
    pub fn record_change(&self, unix_ms: u64) {
        self.inner
            .last_change_unix_ms
            .store(unix_ms.max(1), Ordering::Relaxed);
    }

    /// Membership updates received since the node started.
    pub fn updates_received(&self) -> u64 {
        self.inner.updates_received.load(Ordering::Relaxed)
    }

    /// When membership last changed, Unix milliseconds; `None` before the
    /// first change.
    pub fn last_change_unix_ms(&self) -> Option<u64> {
        match self.inner.last_change_unix_ms.load(Ordering::Relaxed) {
            0 => None,
            unix_ms => Some(unix_ms),
        }
    }
}

/// The wall clock in Unix milliseconds.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_node_has_received_nothing_and_seen_no_change() {
        let stats = GossipStats::default();
        assert_eq!(stats.updates_received(), 0);
        assert_eq!(stats.last_change_unix_ms(), None);
    }

    #[test]
    fn clones_share_the_counters() {
        let stats = GossipStats::default();
        let reader = stats.clone();
        stats.record_updates(3);
        stats.record_updates(2);
        stats.record_change(1_700_000_000_000);
        assert_eq!(reader.updates_received(), 5);
        assert_eq!(reader.last_change_unix_ms(), Some(1_700_000_000_000));
    }
}
