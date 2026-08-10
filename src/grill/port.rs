/// Concurrent-safe ephemeral port allocator.
///
/// Allocates ports from a configurable range using random selection.
/// Thread-safe via `tokio::sync::Mutex` so it can be shared across
/// async tasks without blocking the runtime.
use std::collections::HashSet;
use std::sync::Arc;

use rand::Rng;
use tokio::sync::Mutex;

/// Concurrent-safe ephemeral port allocator.
///
/// Allocates ports from a configurable range (default 10000-60000).
/// Uses random selection to avoid predictable port assignment.
#[derive(Debug, Clone)]
pub struct PortAllocator {
    range_start: u16,
    range_end: u16,
    allocated: Arc<Mutex<HashSet<u16>>>,
}

/// Errors from port allocation operations.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("no ports available in range {start}-{end}")]
    Exhausted { start: u16, end: u16 },

    #[error("port {port} is not currently allocated")]
    NotAllocated { port: u16 },

    #[error("port {port} is already reserved")]
    AlreadyReserved { port: u16 },
}

impl PortAllocator {
    /// Create a new allocator with the given port range (inclusive start, exclusive end).
    pub fn new(range_start: u16, range_end: u16) -> Self {
        Self {
            range_start,
            range_end,
            allocated: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Create from a node config PortRange.
    pub fn from_config(range: &crate::config::node::PortRange) -> Self {
        Self::new(range.start, range.end)
    }

    /// The `(start, end)` port range this allocator draws from. The perimeter
    /// firewall uses it so the ports it drops for outsiders exactly match the
    /// host ports Bun actually allocates, rather than a hardcoded guess.
    pub fn range(&self) -> (u16, u16) {
        (self.range_start, self.range_end)
    }

    /// Allocate a random available port from the range.
    pub async fn allocate(&self) -> Result<u16, PortError> {
        let mut allocated = self.allocated.lock().await;
        // Count only ports inside the allocatable range toward exhaustion (O18):
        // `reserve()` accepts out-of-range ports (adopted workloads), and
        // counting those against the cap made `allocate` report the pool
        // exhausted while in-range ports were still free.
        let in_range = allocated
            .iter()
            .filter(|p| (self.range_start..self.range_end).contains(p))
            .count();
        if in_range >= self.total_ports() {
            return Err(PortError::Exhausted {
                start: self.range_start,
                end: self.range_end,
            });
        }

        let mut rng = rand::thread_rng();
        // Cap retries to avoid spinning when the pool is nearly exhausted.
        for _ in 0..1000 {
            let port = rng.gen_range(self.range_start..self.range_end);
            if allocated.insert(port) {
                return Ok(port);
            }
        }
        Err(PortError::Exhausted {
            start: self.range_start,
            end: self.range_end,
        })
    }

    /// Reserve a specific port (workload adoption after restart/upgrade:
    /// the port is already in use by the surviving instance). Reserving a
    /// port outside the range is allowed — the range only constrains fresh
    /// allocations. Reserving an already-reserved port is an error.
    pub async fn reserve(&self, port: u16) -> Result<(), PortError> {
        let mut allocated = self.allocated.lock().await;
        if allocated.insert(port) {
            Ok(())
        } else {
            Err(PortError::AlreadyReserved { port })
        }
    }

    /// Release a previously allocated port back to the pool.
    pub async fn release(&self, port: u16) -> Result<(), PortError> {
        let mut allocated = self.allocated.lock().await;
        if allocated.remove(&port) {
            Ok(())
        } else {
            Err(PortError::NotAllocated { port })
        }
    }

    /// Check if a port is currently allocated.
    pub async fn is_allocated(&self, port: u16) -> bool {
        let allocated = self.allocated.lock().await;
        allocated.contains(&port)
    }

    /// Number of currently allocated ports.
    pub async fn allocated_count(&self) -> usize {
        let allocated = self.allocated.lock().await;
        allocated.len()
    }

    /// Total number of ports in the range. `0` for an empty/inverted range
    /// (config validation rejects `start >= end`, but saturate defensively so
    /// a stray direct constructor can't underflow).
    pub fn total_ports(&self) -> usize {
        self.range_end.saturating_sub(self.range_start) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allocate_returns_port_in_range() {
        let alloc = PortAllocator::new(10000, 10010);
        let port = alloc.allocate().await.unwrap();
        assert!((10000..10010).contains(&port));
    }

    #[test]
    fn range_reports_the_allocatable_bounds() {
        // The perimeter firewall reads this to drop exactly the host ports the
        // allocator hands out, so it must echo the configured range.
        assert_eq!(PortAllocator::new(10000, 60000).range(), (10000, 60000));
        assert_eq!(PortAllocator::new(40000, 41000).range(), (40000, 41000));
    }

    #[tokio::test]
    async fn allocate_no_duplicates() {
        let alloc = PortAllocator::new(10000, 10100);
        let mut ports = HashSet::new();
        for _ in 0..50 {
            let port = alloc.allocate().await.unwrap();
            assert!(ports.insert(port), "duplicate port {port}");
        }
    }

    #[tokio::test]
    async fn release_makes_port_available() {
        let alloc = PortAllocator::new(10000, 10001);
        let port = alloc.allocate().await.unwrap();
        assert_eq!(port, 10000);

        assert!(alloc.allocate().await.is_err());

        alloc.release(port).await.unwrap();
        let port2 = alloc.allocate().await.unwrap();
        assert_eq!(port2, 10000);
    }

    #[tokio::test]
    async fn allocate_exhausts_range() {
        let alloc = PortAllocator::new(10000, 10003);
        alloc.allocate().await.unwrap();
        alloc.allocate().await.unwrap();
        alloc.allocate().await.unwrap();
        let err = alloc.allocate().await.unwrap_err();
        assert!(matches!(
            err,
            PortError::Exhausted {
                start: 10000,
                end: 10003
            }
        ));
    }

    #[tokio::test]
    async fn release_unallocated_port_rejected() {
        let alloc = PortAllocator::new(10000, 10010);
        let err = alloc.release(10005).await.unwrap_err();
        assert!(matches!(err, PortError::NotAllocated { port: 10005 }));
    }

    #[tokio::test]
    async fn is_allocated_tracks_state() {
        let alloc = PortAllocator::new(10000, 10010);
        let port = alloc.allocate().await.unwrap();
        assert!(alloc.is_allocated(port).await);

        alloc.release(port).await.unwrap();
        assert!(!alloc.is_allocated(port).await);
    }

    #[tokio::test]
    async fn allocated_count_tracks_correctly() {
        let alloc = PortAllocator::new(10000, 10010);
        assert_eq!(alloc.allocated_count().await, 0);

        let p1 = alloc.allocate().await.unwrap();
        assert_eq!(alloc.allocated_count().await, 1);

        alloc.allocate().await.unwrap();
        assert_eq!(alloc.allocated_count().await, 2);

        alloc.release(p1).await.unwrap();
        assert_eq!(alloc.allocated_count().await, 1);
    }

    #[tokio::test]
    async fn single_port_range() {
        let alloc = PortAllocator::new(5000, 5001);
        let port = alloc.allocate().await.unwrap();
        assert_eq!(port, 5000);
        assert!(alloc.allocate().await.is_err());
    }

    #[tokio::test]
    async fn total_ports_returns_range_size() {
        let alloc = PortAllocator::new(10000, 10050);
        assert_eq!(alloc.total_ports(), 50);
    }

    /// O18: reserving a port outside the range (adopted workload) must not
    /// count against the range's exhaustion cap.
    #[tokio::test]
    async fn out_of_range_reserve_does_not_exhaust_the_pool() {
        let alloc = PortAllocator::new(5000, 5002); // two allocatable ports
        // Reserve several out-of-range ports (as adoption does).
        for p in [80u16, 443, 8080] {
            alloc.reserve(p).await.unwrap();
        }
        // Both in-range ports are still allocatable.
        assert!(alloc.allocate().await.is_ok());
        assert!(alloc.allocate().await.is_ok());
        assert!(alloc.allocate().await.is_err());
    }

    #[tokio::test]
    async fn total_ports_saturates_on_an_inverted_range() {
        let alloc = PortAllocator::new(6000, 5000);
        assert_eq!(alloc.total_ports(), 0);
    }

    #[tokio::test]
    async fn from_config_uses_port_range() {
        let range = crate::config::node::PortRange {
            start: 20000,
            end: 20005,
        };
        let alloc = PortAllocator::from_config(&range);
        let port = alloc.allocate().await.unwrap();
        assert!((20000..20005).contains(&port));
    }

    #[tokio::test]
    async fn concurrent_allocation_no_duplicates() {
        let alloc = PortAllocator::new(10000, 10100);
        let mut handles = Vec::new();

        for _ in 0..50 {
            let alloc_clone = alloc.clone();
            handles.push(tokio::spawn(async move {
                alloc_clone.allocate().await.unwrap()
            }));
        }

        let mut ports = HashSet::new();
        for handle in handles {
            let port = handle.await.unwrap();
            assert!(ports.insert(port), "duplicate port {port}");
        }
        assert_eq!(ports.len(), 50);
    }

    #[tokio::test]
    async fn double_release_rejected() {
        let alloc = PortAllocator::new(10000, 10010);
        let port = alloc.allocate().await.unwrap();
        alloc.release(port).await.unwrap();
        assert!(alloc.release(port).await.is_err());
    }
}
