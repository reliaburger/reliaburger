/// Fault registry — in-memory store for active faults.
///
/// The registry tracks all active faults on a node, supports expiry-based
/// cleanup, and provides lookup/clear operations. It is never persisted
/// to disk — a Bun restart always produces a clean state.
use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::types::{FaultId, FaultRequest, FaultRule, FaultSummary};

/// In-memory registry of active faults on this node.
///
/// Uses a `Vec` for storage (small N, simple iteration) and a
/// `BinaryHeap` min-heap for efficient next-expiry tracking.
#[derive(Debug)]
pub struct FaultRegistry {
    /// All active faults, keyed by ID.
    faults: Vec<FaultRule>,
    /// Min-heap of (expiry_ns, fault_id) for efficient expiry.
    expiry_queue: BinaryHeap<Reverse<(u64, u64)>>,
    /// Next ID to assign.
    next_id: u64,
}

impl FaultRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            faults: Vec::new(),
            expiry_queue: BinaryHeap::new(),
            next_id: 1,
        }
    }

    /// Insert a new fault from a request. Returns the assigned FaultRule.
    pub fn insert(&mut self, request: &FaultRequest) -> FaultRule {
        let id = FaultId(self.next_id);
        self.next_id += 1;

        let mut rule = FaultRule::new(
            id,
            request.fault_type.clone(),
            request.target_service.clone(),
            request.duration,
            request.injected_by.clone(),
        );
        rule.target_instance = request.target_instance.clone();
        rule.target_node = request.target_node.clone();
        rule.reason = request.reason.clone();

        self.expiry_queue
            .push(Reverse((rule.expires_at_ns, rule.id.0)));
        self.faults.push(rule.clone());
        rule
    }

    /// Look up a fault by ID.
    pub fn get(&self, id: FaultId) -> Option<&FaultRule> {
        self.faults.iter().find(|f| f.id == id)
    }

    /// Remove a fault by ID. Returns the removed rule if found.
    pub fn remove(&mut self, id: FaultId) -> Option<FaultRule> {
        if let Some(pos) = self.faults.iter().position(|f| f.id == id) {
            let rule = self.faults.swap_remove(pos);
            // Expiry queue entry becomes stale — handled lazily in drain_expired
            Some(rule)
        } else {
            None
        }
    }

    /// Remove all faults. Returns the removed rules.
    pub fn clear(&mut self) -> Vec<FaultRule> {
        self.expiry_queue.clear();
        std::mem::take(&mut self.faults)
    }

    /// Remove all faults targeting a specific service. Returns removed rules.
    pub fn clear_by_service(&mut self, service: &str) -> Vec<FaultRule> {
        let mut removed = Vec::new();
        self.faults.retain(|f| {
            if f.target_service == service {
                removed.push(f.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Drain all faults that have expired as of `now_ns`.
    ///
    /// Returns the expired rules so the caller can clean up
    /// (delete BPF map entries, kill helper processes, etc.).
    pub fn drain_expired(&mut self, now_ns: u64) -> Vec<FaultRule> {
        let mut expired_ids = Vec::new();

        // Pop entries from the min-heap until we find one that hasn't expired
        while let Some(Reverse((expiry_ns, fault_id))) = self.expiry_queue.peek() {
            if *expiry_ns > now_ns {
                break;
            }
            let fault_id = *fault_id;
            self.expiry_queue.pop();

            // Only collect if the fault still exists (it may have been
            // removed manually via clear/remove)
            if self.faults.iter().any(|f| f.id.0 == fault_id) {
                expired_ids.push(FaultId(fault_id));
            }
        }

        let mut expired = Vec::new();
        for id in expired_ids {
            if let Some(rule) = self.remove(id) {
                expired.push(rule);
            }
        }
        expired
    }

    /// The earliest expiry timestamp among active faults, if any.
    pub fn next_expiry(&self) -> Option<u64> {
        self.faults.iter().map(|f| f.expires_at_ns).min()
    }

    /// Number of active faults.
    pub fn len(&self) -> usize {
        self.faults.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.faults.is_empty()
    }

    /// List all active faults as summaries.
    pub fn list(&self) -> Vec<FaultSummary> {
        self.faults.iter().map(FaultSummary::from).collect()
    }

    /// Iterate over all active fault rules.
    pub fn iter(&self) -> impl Iterator<Item = &FaultRule> {
        self.faults.iter()
    }

    /// Count faults targeting a specific service.
    pub fn count_by_service(&self, service: &str) -> usize {
        self.faults
            .iter()
            .filter(|f| f.target_service == service)
            .count()
    }

    /// Count faults targeting a specific node.
    pub fn count_by_node(&self, node: &str) -> usize {
        self.faults
            .iter()
            .filter(|f| f.target_node.as_deref() == Some(node))
            .count()
    }
}

impl Default for FaultRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::smoker::types::FaultType;

    fn delay_request(service: &str, duration_secs: u64) -> FaultRequest {
        FaultRequest {
            fault_type: FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 0,
            },
            target_service: service.into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(duration_secs),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        }
    }

    fn kill_request(service: &str) -> FaultRequest {
        FaultRequest {
            fault_type: FaultType::Kill { count: 1 },
            target_service: service.into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut reg = FaultRegistry::new();
        let rule = reg.insert(&delay_request("redis", 60));
        assert_eq!(rule.id, FaultId(1));
        assert_eq!(rule.target_service, "redis");

        let found = reg.get(FaultId(1)).unwrap();
        assert_eq!(found.target_service, "redis");

        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn insert_assigns_incrementing_ids() {
        let mut reg = FaultRegistry::new();
        let r1 = reg.insert(&delay_request("a", 10));
        let r2 = reg.insert(&delay_request("b", 10));
        let r3 = reg.insert(&delay_request("c", 10));
        assert_eq!(r1.id, FaultId(1));
        assert_eq!(r2.id, FaultId(2));
        assert_eq!(r3.id, FaultId(3));
    }

    #[test]
    fn remove_by_id() {
        let mut reg = FaultRegistry::new();
        reg.insert(&delay_request("redis", 60));
        reg.insert(&delay_request("api", 60));
        assert_eq!(reg.len(), 2);

        let removed = reg.remove(FaultId(1)).unwrap();
        assert_eq!(removed.target_service, "redis");
        assert_eq!(reg.len(), 1);
        assert!(reg.get(FaultId(1)).is_none());
        assert!(reg.get(FaultId(2)).is_some());
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let mut reg = FaultRegistry::new();
        assert!(reg.remove(FaultId(99)).is_none());
    }

    #[test]
    fn clear_removes_all() {
        let mut reg = FaultRegistry::new();
        reg.insert(&delay_request("redis", 60));
        reg.insert(&delay_request("api", 60));
        reg.insert(&kill_request("web"));

        let removed = reg.clear();
        assert_eq!(removed.len(), 3);
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn clear_by_service() {
        let mut reg = FaultRegistry::new();
        reg.insert(&delay_request("redis", 60));
        reg.insert(&kill_request("redis"));
        reg.insert(&delay_request("api", 60));

        let removed = reg.clear_by_service("redis");
        assert_eq!(removed.len(), 2);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.iter().next().unwrap().target_service, "api");
    }

    #[test]
    fn clear_by_service_no_match() {
        let mut reg = FaultRegistry::new();
        reg.insert(&delay_request("redis", 60));
        let removed = reg.clear_by_service("postgres");
        assert!(removed.is_empty());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn expiry_drains_correct_faults() {
        let mut reg = FaultRegistry::new();

        // Insert 3 faults with different expiries
        let r1 = reg.insert(&delay_request("a", 10)); // expires at T+10s
        let _r2 = reg.insert(&delay_request("b", 60)); // expires at T+60s
        let _r3 = reg.insert(&delay_request("c", 5)); // expires at T+5s

        // Drain at a time after r3 and r1 expire, but before r2
        let drain_time = r1.expires_at_ns + 1; // just after r1 expires
        let expired = reg.drain_expired(drain_time);

        // r3 (5s) and r1 (10s) should both be expired
        assert_eq!(expired.len(), 2);
        let expired_services: Vec<_> = expired.iter().map(|f| f.target_service.as_str()).collect();
        assert!(expired_services.contains(&"a"));
        assert!(expired_services.contains(&"c"));

        // r2 (60s) should still be active
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.iter().next().unwrap().target_service, "b");
    }

    #[test]
    fn drain_expired_handles_already_removed() {
        let mut reg = FaultRegistry::new();
        let r1 = reg.insert(&delay_request("a", 5));

        // Remove it manually first
        reg.remove(FaultId(1));

        // Then try to drain — should not panic or return anything
        let expired = reg.drain_expired(r1.expires_at_ns + 1);
        assert!(expired.is_empty());
    }

    #[test]
    fn next_expiry_returns_earliest() {
        let mut reg = FaultRegistry::new();
        let r1 = reg.insert(&delay_request("a", 60));
        let r2 = reg.insert(&delay_request("b", 10));
        let _r3 = reg.insert(&delay_request("c", 30));

        let next = reg.next_expiry().unwrap();
        // r2 has the shortest duration, so it expires first
        assert_eq!(next, r2.expires_at_ns);

        // Remove r2, next should be one of the remaining faults (r3 at T+30 or r1 at T+60)
        reg.remove(FaultId(2));
        let next = reg.next_expiry().unwrap();
        // r3 (30s) expires before r1 (60s), so next should be r3's expiry
        let remaining_expiries: Vec<u64> = reg.iter().map(|f| f.expires_at_ns).collect();
        assert!(remaining_expiries.contains(&next));
        assert!(next < r1.expires_at_ns);
    }

    #[test]
    fn next_expiry_empty_returns_none() {
        let reg = FaultRegistry::new();
        assert!(reg.next_expiry().is_none());
    }

    #[test]
    fn list_returns_summaries() {
        let mut reg = FaultRegistry::new();
        reg.insert(&delay_request("redis", 60));
        reg.insert(&kill_request("web"));

        let summaries = reg.list();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].target_service, "redis");
        assert_eq!(summaries[1].target_service, "web");
    }

    #[test]
    fn count_by_service() {
        let mut reg = FaultRegistry::new();
        reg.insert(&delay_request("redis", 60));
        reg.insert(&kill_request("redis"));
        reg.insert(&delay_request("api", 60));

        assert_eq!(reg.count_by_service("redis"), 2);
        assert_eq!(reg.count_by_service("api"), 1);
        assert_eq!(reg.count_by_service("web"), 0);
    }

    #[test]
    fn count_by_node() {
        let mut reg = FaultRegistry::new();

        let mut req = delay_request("redis", 60);
        req.target_node = Some("node-1".into());
        reg.insert(&req);

        req.target_node = Some("node-1".into());
        reg.insert(&req);

        req.target_node = Some("node-2".into());
        reg.insert(&req);

        assert_eq!(reg.count_by_node("node-1"), 2);
        assert_eq!(reg.count_by_node("node-2"), 1);
        assert_eq!(reg.count_by_node("node-3"), 0);
    }
}
