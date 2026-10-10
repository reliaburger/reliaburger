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
    /// The part of `used` held by task attempts and reusable executors.
    /// Applications report their own requests per instance; this is what
    /// the leader can't see any other way.
    job_used: Resources,
    next_ticket: u64,
    waiters: VecDeque<u64>,
}

/// Node-local scheduling boundary shared by the supervisor and task executors.
#[derive(Debug)]
pub struct ExecutionBudget {
    state: Mutex<State>,
    changed: Notify,
}

/// Who holds a lease: an application replica or job work (a task attempt
/// or a reusable executor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseOwner {
    Application,
    Job,
}

/// Capacity held until the original execution has been retired.
#[derive(Debug)]
pub struct ResourceLease {
    budget: Arc<ExecutionBudget>,
    resources: Resources,
    owner: LeaseOwner,
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
                job_used: Resources::default(),
                next_ticket: 0,
                waiters: VecDeque::new(),
            }),
            changed: Notify::new(),
        })
    }
    /// Refresh capacity without forgiving any existing commitments.
    pub fn set_capacity(&self, capacity: Resources) {
        self.state.lock().expect("budget poisoned").capacity = capacity;
        self.changed.notify_waiters();
    }
    /// Allocatable capacity, independent of currently owned executions.
    pub fn capacity(&self) -> Resources {
        self.state.lock().expect("budget poisoned").capacity
    }
    /// Free resources, including pending and stopping application commitments.
    pub fn available(&self) -> Resources {
        let state = self.state.lock().expect("budget poisoned");
        state.capacity.saturating_sub(&state.used)
    }
    /// Capacity held by task attempts and reusable executors, quarantined
    /// ones included. Application leases are left out: each node reports its
    /// apps' requests per instance, so counting them here would count twice.
    pub fn job_reservations(&self) -> Resources {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .job_used
    }
    /// Idle executors yield their reservations to queued attempts.
    #[cfg(target_os = "linux")]
    pub(crate) fn has_waiters(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waiters
            .is_empty()
    }
    /// Commit an application replica's request in all dimensions
    /// atomically, or leave the ledger unchanged.
    pub fn try_acquire(self: &Arc<Self>, resources: Resources) -> Option<ResourceLease> {
        let mut state = self.state.lock().expect("budget poisoned");
        if !state.capacity.saturating_sub(&state.used).fits(&resources) {
            return None;
        }
        state.used = state.used.saturating_add(&resources);
        Some(ResourceLease {
            budget: Arc::clone(self),
            resources,
            owner: LeaseOwner::Application,
            release_on_drop: true,
        })
    }
    /// Executor admission must not overtake an already queued owner.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn try_acquire_executor(
        self: &Arc<Self>,
        resources: Resources,
    ) -> Option<ResourceLease> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.waiters.is_empty() || !state.capacity.saturating_sub(&state.used).fits(&resources)
        {
            return None;
        }
        state.used = state.used.saturating_add(&resources);
        state.job_used = state.job_used.saturating_add(&resources);
        Some(ResourceLease {
            budget: self.clone(),
            resources,
            owner: LeaseOwner::Job,
            release_on_drop: true,
        })
    }
    /// Reconstruct an existing application replica's commitment even after
    /// capacity shrinks. Recovery cannot forgive running work merely because
    /// it no longer fits.
    pub fn adopt(self: &Arc<Self>, resources: Resources) -> ResourceLease {
        let mut state = self.state.lock().expect("budget poisoned");
        state.used = state.used.saturating_add(&resources);
        ResourceLease {
            budget: self.clone(),
            resources,
            owner: LeaseOwner::Application,
            release_on_drop: true,
        }
    }
    /// Wait for capacity for a task attempt or executor without holding a
    /// partial CPU or memory reservation.
    pub async fn acquire(
        self: &Arc<Self>,
        resources: Resources,
        cancel: &CancellationToken,
    ) -> Option<ResourceLease> {
        let ticket = {
            let mut state = self.state.lock().expect("budget poisoned");
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
                let mut state = self.state.lock().expect("budget poisoned");
                if state.waiters.front() == Some(&ticket)
                    && state.capacity.saturating_sub(&state.used).fits(&resources)
                {
                    state.used = state.used.saturating_add(&resources);
                    state.job_used = state.job_used.saturating_add(&resources);
                    return Some(ResourceLease {
                        budget: self.clone(),
                        resources,
                        owner: LeaseOwner::Job,
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
            .state
            .lock()
            .expect("budget poisoned")
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
        let mut state = self.budget.state.lock().expect("budget poisoned");
        state.used = state.used.saturating_sub(&self.resources);
        if self.owner == LeaseOwner::Job {
            state.job_used = state.job_used.saturating_sub(&self.resources);
        }
        drop(state);
        self.budget.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn job_reservations_count_task_and_executor_leases_but_not_apps() {
        let budget = ExecutionBudget::new(Resources::new(8000, 16 << 30, 0));
        let app = budget
            .try_acquire(Resources::new(1000, 1 << 30, 0))
            .unwrap();
        let adopted = budget.adopt(Resources::new(500, 1 << 30, 0));
        let attempt = budget
            .acquire(Resources::new(2000, 2 << 30, 0), &CancellationToken::new())
            .await
            .unwrap();
        let executor = budget
            .try_acquire_executor(Resources::new(250, 256 << 20, 0))
            .unwrap();
        assert_eq!(
            budget.job_reservations(),
            Resources::new(2250, (2 << 30) + (256 << 20), 0)
        );
        drop(attempt);
        assert_eq!(budget.job_reservations(), Resources::new(250, 256 << 20, 0));
        // An abandoned attempt's capacity stays held, and so does its share.
        drop(
            budget
                .acquire(Resources::new(100, 100, 0), &CancellationToken::new())
                .await
                .unwrap()
                .quarantine_on_drop(),
        );
        assert_eq!(
            budget.job_reservations(),
            Resources::new(350, (256 << 20) + 100, 0)
        );
        drop((app, adopted, executor));
        assert_eq!(budget.job_reservations(), Resources::new(100, 100, 0));
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
