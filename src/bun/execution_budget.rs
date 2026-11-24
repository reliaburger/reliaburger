//! One admission ledger for application commitments and running batch attempts.
//!
//! Requests reserve capacity; limits belong to the runtime. Application leases
//! survive stopping until retirement. Retry backoff and queued chunks own none.
//!
//! An application refused for lack of room leaves a claim on what job work
//! holds (#1967). While the claim is unmet, idle executors retire and busy
//! ones retire after their command, and no new job work takes the room it
//! names. Running work is never stopped for it.
use crate::meat::{AppId, Resources};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long an application's claim holds room against job work, from its
/// first refusal (#1967). The placement reconciler retries a yielding
/// refusal every few seconds inside it. A claim that lapses unmet gives
/// way to a full refusal the leader hears, so it moves the replica.
pub const CLAIM_PATIENCE: Duration = Duration::from_secs(60);

/// How long an application whose claim lapsed unmet gets full refusals on
/// this node, so its claim can't come straight back and hold job work back
/// on a rolling minute for ever.
pub const CLAIM_COOLDOWN: Duration = Duration::from_secs(300);

/// What a capacity refusal of an application means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityRefusalKind {
    /// The app would fit if job work gave its room back, and it's doing so:
    /// the app holds a claim, and its retry here will find the room.
    Yielding,
    /// No amount of job work leaving makes it fit (or its claim lapsed):
    /// the replica belongs on another node.
    Full,
}

/// Room an application was refused, and when the claim lapses.
#[derive(Debug, Clone, Copy)]
struct Claim {
    resources: Resources,
    deadline: Instant,
}

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
    /// Applications refused here for room job work holds.
    claims: HashMap<AppId, Claim>,
    /// Applications whose claim lapsed unmet, until they may claim again.
    cooldowns: HashMap<AppId, Instant>,
}

impl State {
    /// Drop lapsed claims, putting their apps on cooldown, and finished
    /// cooldowns.
    fn expire(&mut self, now: Instant) {
        let lapsed: Vec<(AppId, Instant)> = self
            .claims
            .iter()
            .filter(|(_, claim)| claim.deadline <= now)
            .map(|(app, claim)| (app.clone(), claim.deadline))
            .collect();
        for (app, deadline) in lapsed {
            self.claims.remove(&app);
            self.cooldowns.insert(app, deadline + CLAIM_COOLDOWN);
        }
        self.cooldowns.retain(|_, until| *until > now);
    }
    /// What live claims ask others to leave free, leaving out `except`'s.
    fn claimed(&mut self, now: Instant, except: Option<&AppId>) -> Resources {
        self.expire(now);
        self.claims
            .iter()
            .filter(|(app, _)| Some(*app) != except)
            .fold(Resources::default(), |sum, (_, claim)| {
                sum.saturating_add(&claim.resources)
            })
    }
    /// Free room after `except`'s competitors' claims.
    fn available_to(&mut self, now: Instant, except: Option<&AppId>) -> Resources {
        let claimed = self.claimed(now, except);
        self.capacity
            .saturating_sub(&self.used)
            .saturating_sub(&claimed)
    }
    /// The earliest moment a claim lapses, when queued job work may fit.
    fn next_lapse(&self) -> Option<Instant> {
        self.claims.values().map(|claim| claim.deadline).min()
    }
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
                claims: HashMap::new(),
                cooldowns: HashMap::new(),
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
    /// Capacity held by task attempts and reusable executors, quarantined
    /// ones included. Application leases are left out: each node reports its
    /// apps' requests per instance, so counting them here would count twice.
    pub fn job_reservations(&self) -> Resources {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .job_used
    }
    /// Free resources job work may take: [`Self::available`] less what
    /// refused applications claim.
    pub fn job_available(&self) -> Resources {
        self.lock().available_to(Instant::now(), None)
    }
    /// Idle executors yield their reservations to queued attempts, and to
    /// application claims the free room doesn't cover yet.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn has_waiters(&self) -> bool {
        let mut state = self.lock();
        if !state.waiters.is_empty() {
            return true;
        }
        let claimed = state.claimed(Instant::now(), None);
        !state.capacity.saturating_sub(&state.used).fits(&claimed)
    }
    /// Record that `app` was refused `resources` here (its whole share on
    /// this node), and say what kind of refusal that is (#1967).
    ///
    /// It's [`CapacityRefusalKind::Yielding`] when job work leaving would
    /// make the share fit beside other apps and their claims: the app then
    /// holds a claim that job admission leaves free. A second refusal
    /// updates the share but keeps the first deadline, [`CLAIM_PATIENCE`]
    /// after it. Otherwise, or while the app is on cooldown after a claim
    /// that lapsed, it's [`CapacityRefusalKind::Full`] and claims nothing.
    pub fn claim_for_app(&self, app: &AppId, resources: Resources) -> CapacityRefusalKind {
        let now = Instant::now();
        let mut state = self.lock();
        let others = state.claimed(now, Some(app));
        let apps_used = state.used.saturating_sub(&state.job_used);
        let room = state
            .capacity
            .saturating_sub(&apps_used)
            .saturating_sub(&others);
        if state.cooldowns.contains_key(app) || !room.fits(&resources) {
            state.claims.remove(app);
            return CapacityRefusalKind::Full;
        }
        let deadline = state
            .claims
            .get(app)
            .map_or(now + CLAIM_PATIENCE, |claim| claim.deadline);
        state.claims.insert(
            app.clone(),
            Claim {
                resources,
                deadline,
            },
        );
        CapacityRefusalKind::Yielding
    }
    /// Forget `app`'s claim and cooldown: it was admitted here. Admission is
    /// the only thing that clears a cooldown; leaving the node uses
    /// [`Self::withdraw_app_claim`].
    pub fn release_app_claim(&self, app: &AppId) {
        let mut state = self.lock();
        state.cooldowns.remove(app);
        let released = state.claims.remove(app).is_some();
        drop(state);
        if released {
            self.changed.notify_waiters();
        }
    }
    /// Drop `app`'s claim because it left this node, keeping any cooldown.
    /// A full refusal makes the leader move the app, so the node retires
    /// it; clearing the cooldown here would let the yield pass put it
    /// straight back with a fresh claim, and job work would rotate through
    /// minute-long holds for ever. A claim that has lapsed becomes its
    /// cooldown first, even if nothing read the ledger since.
    pub fn withdraw_app_claim(&self, app: &AppId) {
        let mut state = self.lock();
        state.expire(Instant::now());
        let released = state.claims.remove(app).is_some();
        drop(state);
        if released {
            self.changed.notify_waiters();
        }
    }
    /// Commit an application replica's request in all dimensions
    /// atomically, or leave the ledger unchanged. Room other applications
    /// claim is off limits; `app`'s own claim is what it may use.
    pub fn try_acquire_app(
        self: &Arc<Self>,
        app: &AppId,
        resources: Resources,
    ) -> Option<ResourceLease> {
        self.acquire_application(Some(app), resources)
    }
    /// Commit a request in all dimensions atomically, or leave the ledger
    /// unchanged, outside every application's claim.
    pub fn try_acquire(self: &Arc<Self>, resources: Resources) -> Option<ResourceLease> {
        self.acquire_application(None, resources)
    }
    fn acquire_application(
        self: &Arc<Self>,
        app: Option<&AppId>,
        resources: Resources,
    ) -> Option<ResourceLease> {
        let mut state = self.lock();
        if !state.available_to(Instant::now(), app).fits(&resources) {
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
    /// Executor admission must not overtake an already queued owner, nor
    /// take room an application claims.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn try_acquire_executor(
        self: &Arc<Self>,
        resources: Resources,
    ) -> Option<ResourceLease> {
        let mut state = self.lock();
        if !state.waiters.is_empty() || !state.available_to(Instant::now(), None).fits(&resources) {
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
        let mut state = self.lock();
        state.used = state.used.saturating_add(&resources);
        ResourceLease {
            budget: self.clone(),
            resources,
            owner: LeaseOwner::Application,
            release_on_drop: true,
        }
    }
    /// Wait for capacity for a task attempt or executor without holding a
    /// partial CPU or memory reservation, or taking room an application
    /// claims.
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
            let lapse = {
                let mut state = self.lock();
                if state.waiters.front() == Some(&ticket)
                    && state.available_to(Instant::now(), None).fits(&resources)
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
                state.next_lapse()
            };
            // A lapsing claim changes nothing anyone is notified of, so
            // wake for it too.
            let lapsed = async {
                match lapse {
                    Some(until) => tokio::time::sleep_until(until).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {}, () = lapsed => {} }
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
    fn web() -> AppId {
        AppId::new("web", "default")
    }

    fn db() -> AppId {
        AppId::new("db", "default")
    }

    fn of(cpu: u64, memory: u64) -> Resources {
        Resources::new(cpu, memory, 0)
    }

    /// Poll `future` once, expecting it to be still waiting.
    async fn still_waiting<F: std::future::Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[test]
    fn a_refused_app_that_would_fit_without_jobs_records_a_claim() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let executor = budget.try_acquire_executor(of(800, 800)).unwrap();
        assert!(budget.try_acquire_app(&web(), of(500, 500)).is_none());

        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Yielding
        );
        assert_eq!(budget.job_available(), Resources::default());
        drop(executor);
        assert_eq!(budget.job_available(), of(500, 524));
    }

    #[test]
    fn a_refused_app_that_wouldnt_fit_even_without_jobs_is_refused_as_full() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let other = budget.try_acquire_app(&db(), of(800, 800)).unwrap();
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Full
        );
        assert!(!budget.has_waiters());
        assert_eq!(budget.job_available(), budget.available());
        // Bigger than the node: full, whatever job work does.
        drop(other);
        assert_eq!(
            budget.claim_for_app(&web(), of(2000, 500)),
            CapacityRefusalKind::Full
        );
    }

    /// The 0.3.0 rehearsal's failure in miniature (#1967): a job lease is
    /// released while an attempt waits in the FIFO. The attempt used to
    /// take the room; now the claiming app's retry gets it.
    #[tokio::test]
    async fn a_released_job_lease_goes_to_the_claiming_app_not_the_queued_attempt() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let running = budget
            .acquire(of(1000, 1000), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Yielding
        );
        let cancel = CancellationToken::new();
        let queued = budget.acquire(of(600, 600), &cancel);
        tokio::pin!(queued);
        still_waiting(queued.as_mut()).await;

        drop(running);
        still_waiting(queued.as_mut()).await;
        let app = budget.try_acquire_app(&web(), of(500, 500)).unwrap();
        budget.release_app_claim(&web());
        still_waiting(queued.as_mut()).await;
        drop(app);
        let attempt = tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .unwrap()
            .unwrap();
        drop(attempt);
    }

    #[test]
    fn try_acquire_executor_leaves_claimed_room_alone() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        budget.claim_for_app(&web(), of(600, 600));
        assert!(budget.try_acquire_executor(of(500, 500)).is_none());
        drop(budget.try_acquire_executor(of(400, 400)).unwrap());
    }

    /// Another app's claim is off limits to an app too, so a second refused
    /// app can't take the room the first is waiting for.
    #[test]
    fn an_app_uses_its_own_claim_but_not_anothers() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        budget.claim_for_app(&web(), of(600, 600));
        assert!(budget.try_acquire_app(&db(), of(500, 500)).is_none());
        assert!(budget.try_acquire(of(500, 500)).is_none());
        drop(budget.try_acquire_app(&web(), of(600, 600)).unwrap());
    }

    #[test]
    fn jobs_yield_only_while_claims_exceed_free_memory() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let first = budget.try_acquire_executor(of(400, 400)).unwrap();
        let second = budget.try_acquire_executor(of(400, 400)).unwrap();
        assert!(!budget.has_waiters(), "nothing asked for room yet");
        budget.claim_for_app(&web(), of(500, 500));
        assert!(budget.has_waiters(), "200m free can't cover a 500m claim");
        drop(first);
        assert!(
            !budget.has_waiters(),
            "600m free covers it: the other executor stays warm"
        );
        drop(second);
    }

    /// An app the leader moved away never retries here. Its claim lapses
    /// after its patience, and queued job work wakes for that alone.
    #[tokio::test(start_paused = true)]
    async fn a_claim_lapses_after_its_patience_and_wakes_job_waiters() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        budget.claim_for_app(&web(), of(600, 600));
        let cancel = CancellationToken::new();
        let attempt = budget.acquire(of(500, 500), &cancel);
        tokio::pin!(attempt);
        still_waiting(attempt.as_mut()).await;
        tokio::time::advance(CLAIM_PATIENCE + Duration::from_millis(1)).await;
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready(attempt.as_mut().poll(cx))).await;
        let std::task::Poll::Ready(Some(lease)) = polled else {
            panic!("the attempt still waits behind a lapsed claim");
        };
        drop(lease);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lapsed_claim_puts_its_app_on_cooldown() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let executor = budget.try_acquire_executor(of(1000, 1000)).unwrap();
        budget.claim_for_app(&web(), of(500, 500));
        tokio::time::advance(CLAIM_PATIENCE).await;
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Full,
            "a lapsed claim can't come straight back"
        );
        assert!(!budget.has_waiters());
        tokio::time::advance(CLAIM_COOLDOWN).await;
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Yielding
        );
        drop(executor);
    }

    /// Review of #2250: every full refusal makes the leader move the app,
    /// so the node retires it. Retiring used to clear the cooldown too, and
    /// the yield pass could put the replica straight back with a fresh
    /// claim, rotating job work through minute-long holds for ever.
    #[tokio::test(start_paused = true)]
    async fn leaving_the_node_keeps_a_lapsed_claims_cooldown() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let executor = budget.try_acquire_executor(of(1000, 1000)).unwrap();
        budget.claim_for_app(&web(), of(500, 500));
        tokio::time::advance(CLAIM_PATIENCE).await;
        budget.withdraw_app_claim(&web());
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Full,
            "the app came back within its cooldown and claimed again"
        );
        tokio::time::advance(CLAIM_COOLDOWN).await;
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Yielding
        );
        drop(executor);
    }

    /// Admission is the one thing that forgives a cooldown.
    #[tokio::test(start_paused = true)]
    async fn admission_clears_a_cooldown() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        let executor = budget.try_acquire_executor(of(1000, 1000)).unwrap();
        budget.claim_for_app(&web(), of(500, 500));
        tokio::time::advance(CLAIM_PATIENCE).await;
        budget.release_app_claim(&web());
        assert_eq!(
            budget.claim_for_app(&web(), of(500, 500)),
            CapacityRefusalKind::Yielding
        );
        drop(executor);
    }

    #[test]
    fn removing_an_app_drops_its_claim() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        budget.claim_for_app(&web(), of(600, 600));
        budget.withdraw_app_claim(&web());
        assert_eq!(budget.job_available(), budget.available());
    }

    /// Recovery restores a surviving app's lease even into claimed room: a
    /// running app outranks one that hasn't started.
    #[test]
    fn adopt_ignores_claims() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        budget.claim_for_app(&web(), of(600, 600));
        let adopted = budget.adopt(of(1000, 1000));
        assert_eq!(budget.available(), of(0, 24));
        drop(adopted);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_refusal_of_the_same_app_replaces_its_claim_and_keeps_the_deadline() {
        let budget = ExecutionBudget::new(of(1000, 1024));
        budget.claim_for_app(&web(), of(300, 300));
        tokio::time::advance(CLAIM_PATIENCE / 2).await;
        budget.claim_for_app(&web(), of(600, 600));
        assert_eq!(budget.job_available(), of(400, 424));
        tokio::time::advance(CLAIM_PATIENCE / 2).await;
        assert_eq!(
            budget.job_available(),
            budget.available(),
            "the deadline ran from the first refusal"
        );
    }

    proptest::proptest! {
        /// Whatever apps and job work do, no job admission lands in room an
        /// app claims, and claims never exceed what apps leave.
        #[test]
        fn job_admission_never_lands_in_claimed_room(
            steps in proptest::collection::vec((0u8..5, 1u64..600), 1..60)
        ) {
            let budget = ExecutionBudget::new(of(1000, 1000));
            let apps = [web(), db()];
            let mut leases = Vec::new();
            for (step, size) in steps {
                let request = of(size, size);
                let app = &apps[size as usize % 2];
                match step {
                    0 => {
                        let before = budget.job_available();
                        if let Some(lease) = budget.try_acquire_executor(request) {
                            proptest::prop_assert!(before.fits(&request));
                            leases.push(lease);
                        }
                    }
                    1 => match budget.try_acquire_app(app, request) {
                        Some(lease) => {
                            budget.release_app_claim(app);
                            leases.push(lease);
                        }
                        None => {
                            budget.claim_for_app(app, request);
                        }
                    },
                    2 => {
                        if !leases.is_empty() {
                            leases.remove(size as usize % leases.len());
                        }
                    }
                    3 => budget.release_app_claim(app),
                    _ => {}
                }
                let mut state = budget.lock();
                let apps_used = state.used.saturating_sub(&state.job_used);
                let claimed = state.claimed(Instant::now(), None);
                proptest::prop_assert!(
                    state.capacity.saturating_sub(&apps_used).fits(&claimed)
                );
            }
        }
    }

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
