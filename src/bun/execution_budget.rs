//! One admission ledger for application commitments and running batch attempts.
//!
//! Requests reserve capacity; limits belong to the runtime. Application leases
//! survive stopping until retirement. Retry backoff and queued chunks own none.
use crate::meat::Resources;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct State {
    capacity: Resources,
    used: Resources,
    next_ticket: u64,
    waiters: VecDeque<u64>,
}

/// Node-local scheduling boundary shared by the supervisor and task executors.
#[derive(Debug)]
pub struct ExecutionBudget {
    state: Mutex<State>,
    changed: Notify,
}

/// Capacity held until the original execution has been retired.
#[derive(Debug)]
pub struct ResourceLease {
    budget: Arc<ExecutionBudget>,
    resources: Resources,
    release_on_drop: bool,
}

impl ResourceLease {
    /// An abandoned execution cannot return capacity without retirement evidence.
    pub(crate) fn quarantine_on_drop(mut self) -> Self {
        self.release_on_drop = false;
        self
    }
    pub(crate) fn confirm_retired(&mut self) {
        self.release_on_drop = true;
    }
}

impl ExecutionBudget {
    /// Capacity after the operating system and agent reservation.
    pub fn new(capacity: Resources) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                capacity,
                used: Resources::default(),
                next_ticket: 0,
                waiters: VecDeque::new(),
            }),
            changed: Notify::new(),
        })
    }
    /// The ledger. Every update leaves it consistent before anything that
    /// could panic, so a lock poisoned by an unrelated panic still holds a
    /// valid ledger, and a lease's `Drop` must never panic on it.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Refresh capacity without forgiving any existing commitments.
    pub fn set_capacity(&self, capacity: Resources) {
        self.lock().capacity = capacity;
        self.changed.notify_waiters();
    }
    /// Allocatable capacity, independent of currently owned executions.
    pub fn capacity(&self) -> Resources {
        self.lock().capacity
    }
    /// Free resources, including pending and stopping application commitments.
    pub fn available(&self) -> Resources {
        let state = self.lock();
        state.capacity.saturating_sub(&state.used)
    }
    /// Idle executors yield their reservations to queued attempts.
    #[cfg(target_os = "linux")]
    pub(crate) fn has_waiters(&self) -> bool {
        !self.lock().waiters.is_empty()
    }
    /// Commit all dimensions atomically, or leave the ledger unchanged.
    pub fn try_acquire(self: &Arc<Self>, resources: Resources) -> Option<ResourceLease> {
        let mut state = self.lock();
        if !state.capacity.saturating_sub(&state.used).fits(&resources) {
            return None;
        }
        state.used = state.used.saturating_add(&resources);
        Some(ResourceLease {
            budget: Arc::clone(self),
            resources,
            release_on_drop: true,
        })
    }
    /// Executor admission must not overtake an already queued owner.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn try_acquire_executor(
        self: &Arc<Self>,
        resources: Resources,
    ) -> Option<ResourceLease> {
        let mut state = self.lock();
        if !state.waiters.is_empty() || !state.capacity.saturating_sub(&state.used).fits(&resources)
        {
            return None;
        }
        state.used = state.used.saturating_add(&resources);
        Some(ResourceLease {
            budget: self.clone(),
            resources,
            release_on_drop: true,
        })
    }
    /// Reconstruct an existing owner's commitment even after capacity shrinks.
    /// Recovery cannot forgive running work merely because it no longer fits.
    pub fn adopt(self: &Arc<Self>, resources: Resources) -> ResourceLease {
        let mut state = self.lock();
        state.used = state.used.saturating_add(&resources);
        ResourceLease {
            budget: self.clone(),
            resources,
            release_on_drop: true,
        }
    }
    /// Wait for capacity without holding a partial CPU or memory reservation.
    pub async fn acquire(
        self: &Arc<Self>,
        resources: Resources,
        cancel: &CancellationToken,
    ) -> Option<ResourceLease> {
        let ticket = {
            let mut state = self.lock();
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.waiters.push_back(ticket);
            ticket
        };
        let _waiting = Waiting {
            budget: self.clone(),
            ticket,
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if cancel.is_cancelled() {
                return None;
            }
            {
                let mut state = self.lock();
                if state.waiters.front() == Some(&ticket)
                    && state.capacity.saturating_sub(&state.used).fits(&resources)
                {
                    state.used = state.used.saturating_add(&resources);
                    return Some(ResourceLease {
                        budget: self.clone(),
                        resources,
                        release_on_drop: true,
                    });
                }
            }
            tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
        }
    }
}
struct Waiting {
    budget: Arc<ExecutionBudget>,
    ticket: u64,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        self.budget
            .lock()
            .waiters
            .retain(|ticket| *ticket != self.ticket);
        self.budget.changed.notify_waiters();
    }
}
impl Drop for ResourceLease {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let mut state = self.budget.lock();
        state.used = state.used.saturating_sub(&self.resources);
        drop(state);
        self.budget.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn poisoned_budget_lock_does_not_panic_on_drop() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1024, 0));
        let lease = budget.try_acquire(Resources::new(500, 512, 0)).unwrap();
        let poisoner = budget.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.state.lock().unwrap();
            panic!("poison the budget lock");
        })
        .join();
        assert!(budget.state.is_poisoned());
        drop(lease);
        assert_eq!(budget.available(), budget.capacity());
        assert!(budget.try_acquire(Resources::new(1000, 1024, 0)).is_some());
    }
    #[test]
    fn mixed_requests_and_apps_share_all_dimensions() {
        let budget = ExecutionBudget::new(Resources::new(8000, 16 << 30, 0));
        let app = budget
            .try_acquire(Resources::new(2000, 4 << 30, 0))
            .unwrap();
        let large = budget
            .try_acquire(Resources::new(4000, 8 << 30, 0))
            .unwrap();
        let small = budget
            .try_acquire(Resources::new(250, 256 << 20, 0))
            .unwrap();
        assert!(
            budget
                .try_acquire(Resources::new(2000, 1 << 30, 0))
                .is_none()
        );
        assert!(
            budget
                .try_acquire(Resources::new(100, 4 << 30, 0))
                .is_none()
        );
        drop(large);
        assert!(
            budget
                .try_acquire(Resources::new(4000, 8 << 30, 0))
                .is_some()
        );
        drop(small);
        drop(app);
        assert_eq!(budget.available(), Resources::new(8000, 16 << 30, 0));
    }
    #[tokio::test]
    async fn waits_cancel_without_leaking_or_losing_release_notification() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1024, 0));
        let lease = budget.try_acquire(Resources::new(1000, 1024, 0)).unwrap();
        let cancel = CancellationToken::new();
        let waiter = {
            let budget = budget.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { budget.acquire(Resources::new(500, 512, 0), &cancel).await })
        };
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(waiter.await.unwrap().is_none());
        drop(lease);
        assert_eq!(budget.available(), Resources::new(1000, 1024, 0));
    }
    #[test]
    fn reducing_capacity_does_not_erase_owned_resources() {
        let budget = ExecutionBudget::new(Resources::new(2000, 2048, 0));
        let lease = budget.try_acquire(Resources::new(1500, 1024, 0)).unwrap();
        budget.set_capacity(Resources::new(1000, 2048, 0));
        assert_eq!(budget.available().cpu_millicores, 0);
        assert!(budget.try_acquire(Resources::new(1, 1, 0)).is_none());
        drop(lease);
        assert_eq!(budget.available().cpu_millicores, 1000);
    }
    #[tokio::test]
    async fn large_waiter_is_not_overtaken_by_a_smaller_request() {
        let budget = ExecutionBudget::new(Resources::new(2000, 2048, 0));
        let running = budget.try_acquire(Resources::new(1000, 1024, 0)).unwrap();
        let large = {
            let budget = budget.clone();
            tokio::spawn(async move {
                budget
                    .acquire(Resources::new(2000, 2048, 0), &CancellationToken::new())
                    .await
                    .unwrap()
            })
        };
        while budget.state.lock().unwrap().waiters.is_empty() {
            tokio::task::yield_now().await;
        }
        let small = {
            let budget = budget.clone();
            tokio::spawn(async move {
                budget
                    .acquire(Resources::new(500, 512, 0), &CancellationToken::new())
                    .await
                    .unwrap()
            })
        };
        tokio::task::yield_now().await;
        assert!(!small.is_finished());
        drop(running);
        let large = tokio::time::timeout(Duration::from_secs(1), large)
            .await
            .unwrap()
            .unwrap();
        assert!(!small.is_finished());
        drop(large);
        drop(
            tokio::time::timeout(Duration::from_secs(1), small)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(budget.available(), budget.capacity());
    }
    #[tokio::test]
    async fn reusable_admission_respects_queued_owners_and_their_cancellation() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        let budget = ExecutionBudget::new(Resources::new(2000, 2048, 0));
        let running = budget.try_acquire(Resources::new(1000, 1024, 0)).unwrap();
        let cancel = CancellationToken::new();
        let large = budget.acquire(Resources::new(2000, 2048, 0), &cancel);
        tokio::pin!(large);
        poll_fn(|cx| {
            assert!(large.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(
            budget
                .try_acquire_executor(Resources::new(500, 512, 0))
                .is_none(),
            "the immediate executor path cannot skip a larger queued owner"
        );
        cancel.cancel();
        assert!(large.await.is_none());
        let executor = budget
            .try_acquire_executor(Resources::new(500, 512, 0))
            .unwrap();
        drop(executor);
        drop(running);
        assert_eq!(budget.available(), budget.capacity());
    }
    #[test]
    fn abandoned_attempt_keeps_its_capacity_and_adoption_never_forgives_an_owner() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1024, 0));
        drop(
            budget
                .try_acquire(Resources::new(500, 512, 0))
                .unwrap()
                .quarantine_on_drop(),
        );
        assert_eq!(budget.available(), Resources::new(500, 512, 0));
        let mut confirmed = budget
            .try_acquire(Resources::new(500, 512, 0))
            .unwrap()
            .quarantine_on_drop();
        confirmed.confirm_retired();
        drop(confirmed);
        assert_eq!(budget.available(), Resources::new(500, 512, 0));
        let adopted = budget.adopt(Resources::new(2000, 2048, 0));
        assert_eq!(budget.available(), Resources::default());
        drop(adopted);
        assert_eq!(budget.available(), Resources::new(500, 512, 0));
    }
}
