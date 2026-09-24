//! Leases that bound how long a node may route with a view nobody refreshed.
//!
//! Every node that polls the leader for placements is a *consumer* of the
//! endpoint catalogue, and the withdrawal ledger waits for each consumer to
//! confirm that it stopped routing to an address before that address can be
//! reused. A node that is gone can never confirm, so without a bound one
//! stopped node would freeze every address release in the cluster.
//!
//! The bound is a lease with two halves that never talk to each other:
//!
//! - [`ViewLease`] lives on the consumer. Each time the leader answers a
//!   placement poll and the node publishes that answer, the lease runs for
//!   [`CONSUMER_VIEW_LEASE`] from the moment the request was *sent*. When it
//!   runs out, the node stops routing cluster services: Bun withdraws its view,
//!   Wrapper refuses cluster routes and the kernel's connect hook refuses
//!   virtual addresses, even if Bun itself has died.
//! - [`ConsumerContacts`] lives on the leader. It remembers when it last served
//!   each consumer. Only a consumer that has been silent for longer than the
//!   lease plus [`CONSUMER_DISCHARGE_MARGIN`], and that gossip doesn't report
//!   as alive, may be discharged from the ledger.
//!
//! The consumer's lease starts before the leader's clock does and is shorter
//! than the leader's wait, so by the time the leader stops waiting for a node,
//! that node has already stopped routing. Neither side compares wall clocks.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long a consumer may keep routing with the last catalogue it published,
/// measured from the moment it sent the placement request that carried it.
pub const CONSUMER_VIEW_LEASE: Duration = Duration::from_secs(60);

/// How much longer than [`CONSUMER_VIEW_LEASE`] the leader waits before it
/// discharges a silent consumer. It covers clocks running at slightly
/// different rates, the agent loop noticing an expiry a tick late, and a
/// deposed leader that served one last poll while its successor took over.
pub const CONSUMER_DISCHARGE_MARGIN: Duration = Duration::from_secs(20);

/// The silence after which the leader may discharge a consumer that gossip
/// doesn't report as alive.
pub const CONSUMER_DISCHARGE_AFTER: Duration =
    Duration::from_secs(CONSUMER_VIEW_LEASE.as_secs() + CONSUMER_DISCHARGE_MARGIN.as_secs());

/// Nanoseconds on the clock the kernel's lease check reads.
///
/// On Linux this is `CLOCK_BOOTTIME`, the clock behind the eBPF helper
/// `bpf_ktime_get_boot_ns()`. Unlike `CLOCK_MONOTONIC` it keeps counting
/// while the machine is suspended, so a laptop VM that sleeps through its
/// lease wakes up with the lease expired rather than frozen. Off Linux there
/// is no kernel data path, so a process-local epoch is enough.
pub fn boot_clock_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, properly aligned `timespec` that the kernel
        // only writes into; CLOCK_BOOTTIME exists on every supported kernel.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
        if rc == 0 {
            return (ts.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec as u64);
        }
    }
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A consumer's permission to route with its last published cluster view.
///
/// Shared (behind an `Arc`) between the agent, which renews and fences it,
/// and Wrapper, which checks it on every request. Until [`enforce`] is called
/// the lease is always valid: a standalone node or a plaintext development
/// cluster isn't a registered consumer and has nothing to fence.
///
/// [`enforce`]: ViewLease::enforce
#[derive(Debug, Default)]
pub struct ViewLease {
    enforced: AtomicBool,
    /// Expiry on [`boot_clock_ns`]; zero means already expired.
    expires_ns: AtomicU64,
}

impl ViewLease {
    /// Start enforcing the lease, expired until the first renewal.
    pub fn enforce(&self) {
        self.expires_ns.store(0, Ordering::SeqCst);
        self.enforced.store(true, Ordering::SeqCst);
    }

    /// Whether this node is a registered consumer whose view can lapse.
    pub fn is_enforced(&self) -> bool {
        self.enforced.load(Ordering::SeqCst)
    }

    /// Extend the lease to [`CONSUMER_VIEW_LEASE`] after `requested_at_ns`, the
    /// boot-clock time the answered placement request was sent. A late answer
    /// to an older request never shortens a newer lease. Returns the expiry.
    pub fn renew(&self, requested_at_ns: u64) -> u64 {
        let lease = u64::try_from(CONSUMER_VIEW_LEASE.as_nanos()).unwrap_or(u64::MAX);
        let expires = requested_at_ns.saturating_add(lease);
        self.expires_ns
            .fetch_max(expires, Ordering::SeqCst)
            .max(expires)
    }

    /// End the lease now; the view must be withdrawn before routing again.
    pub fn expire(&self) {
        self.expires_ns.store(0, Ordering::SeqCst);
    }

    /// The expiry on [`boot_clock_ns`], or `None` while not enforced.
    pub fn expires_ns(&self) -> Option<u64> {
        self.is_enforced()
            .then(|| self.expires_ns.load(Ordering::SeqCst))
    }

    /// Whether the node may route with its view at boot-clock time `now_ns`.
    pub fn is_valid_at(&self, now_ns: u64) -> bool {
        self.expires_ns().is_none_or(|expires| now_ns < expires)
    }

    /// Whether the node may route with its view now.
    pub fn is_valid(&self) -> bool {
        self.is_valid_at(boot_clock_ns())
    }
}

/// The leader's memory of when it last served each consumer, for this term.
///
/// This is volatile on purpose. A new leader has served nobody yet, so it
/// counts every consumer's silence from the moment it took over: it can only
/// discharge later than a leader with full memory would, never sooner.
#[derive(Debug)]
pub struct ConsumerContacts {
    term: Option<u64>,
    since: Instant,
    served: HashMap<String, Instant>,
    discharging: HashSet<String>,
}

impl ConsumerContacts {
    /// Contacts for a leader that has served nobody yet.
    pub fn new(now: Instant) -> Self {
        Self {
            term: None,
            since: now,
            served: HashMap::new(),
            discharging: HashSet::new(),
        }
    }

    /// Start afresh when the Raft term changes: contacts made under another
    /// term, or by another leader, prove nothing about this one.
    pub fn observe_term(&mut self, term: u64, now: Instant) {
        if self.term != Some(term) {
            *self = Self::new(now);
            self.term = Some(term);
        }
    }

    /// Record that `node` is being served now. Returns `false`, recording
    /// nothing, while a discharge of `node` is in flight: serving it then
    /// could hand out a lease the ledger is about to stop honouring.
    pub fn record(&mut self, node: &str, now: Instant) -> bool {
        if self.discharging.contains(node) {
            return false;
        }
        self.served.insert(node.to_string(), now);
        true
    }

    /// How long `node` has gone without being served by this leader.
    pub fn silence(&self, node: &str, now: Instant) -> Duration {
        let last = self
            .served
            .get(node)
            .copied()
            .map_or(self.since, |served| served.max(self.since));
        now.saturating_duration_since(last)
    }

    /// Registered consumers that have been silent for at least `bound` and
    /// that gossip doesn't report as alive, excluding discharges in flight.
    pub fn lapsed<'a>(
        &self,
        consumers: impl IntoIterator<Item = &'a String>,
        alive: &HashSet<&str>,
        now: Instant,
        bound: Duration,
    ) -> Vec<String> {
        consumers
            .into_iter()
            .filter(|node| !alive.contains(node.as_str()))
            .filter(|node| !self.discharging.contains(node.as_str()))
            .filter(|node| self.silence(node, now) >= bound)
            .cloned()
            .collect()
    }

    /// Refuse to serve `node` until its discharge has a known outcome.
    pub fn begin_discharge(&mut self, node: &str) {
        self.discharging.insert(node.to_string());
    }

    /// Discharges whose outcome is not yet known.
    pub fn discharging(&self) -> impl Iterator<Item = &String> {
        self.discharging.iter()
    }

    /// The discharge of `node` committed or was definitively refused.
    pub fn finish_discharge(&mut self, node: &str) {
        self.discharging.remove(node);
        self.served.remove(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn a_lease_is_always_valid_until_it_is_enforced() {
        let lease = ViewLease::default();
        assert!(lease.is_valid_at(u64::MAX - 1));
        assert_eq!(lease.expires_ns(), None);
        lease.enforce();
        assert!(!lease.is_valid_at(0), "an enforced lease starts expired");
    }

    #[test]
    fn a_renewed_lease_runs_from_the_request_and_never_shrinks() {
        let lease = ViewLease::default();
        lease.enforce();
        let expires = lease.renew(100 * SECOND);
        assert_eq!(expires, 160 * SECOND);
        assert!(lease.is_valid_at(159 * SECOND));
        assert!(!lease.is_valid_at(160 * SECOND));
        // A slow answer to an older request arrives after a newer one.
        assert_eq!(lease.renew(90 * SECOND), 160 * SECOND);
        assert!(lease.is_valid_at(159 * SECOND));
        lease.expire();
        assert!(!lease.is_valid_at(101 * SECOND));
        assert_eq!(lease.renew(200 * SECOND), 260 * SECOND);
    }

    #[test]
    fn the_leader_waits_longer_than_any_consumer_routes() {
        assert!(CONSUMER_DISCHARGE_AFTER > CONSUMER_VIEW_LEASE);
        assert_eq!(
            CONSUMER_DISCHARGE_AFTER,
            CONSUMER_VIEW_LEASE + CONSUMER_DISCHARGE_MARGIN
        );
    }

    fn consumers(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn only_silent_consumers_that_gossip_calls_gone_lapse() {
        let start = Instant::now();
        let mut contacts = ConsumerContacts::new(start);
        contacts.observe_term(3, start);
        let bound = Duration::from_secs(80);
        let all = consumers(&["gone", "partitioned-alive", "polling"]);
        let alive = HashSet::from(["partitioned-alive", "polling"]);
        assert!(contacts.record("gone", start));
        assert!(contacts.record("polling", start + Duration::from_secs(70)));

        let early = start + Duration::from_secs(79);
        assert!(contacts.lapsed(&all, &alive, early, bound).is_empty());
        let late = start + Duration::from_secs(80);
        assert_eq!(contacts.lapsed(&all, &alive, late, bound), ["gone"]);
        // Alive in gossip is never discharged, however long it is silent.
        let much_later = start + Duration::from_secs(1_000);
        assert_eq!(contacts.lapsed(&all, &alive, much_later, bound), ["gone"]);
    }

    #[test]
    fn a_new_term_counts_silence_from_the_takeover() {
        let start = Instant::now();
        let mut contacts = ConsumerContacts::new(start);
        contacts.observe_term(3, start);
        let bound = Duration::from_secs(80);
        let all = consumers(&["gone"]);
        let alive = HashSet::new();
        let takeover = start + Duration::from_secs(100);
        contacts.observe_term(4, takeover);
        assert!(
            contacts
                .lapsed(&all, &alive, takeover + Duration::from_secs(79), bound)
                .is_empty()
        );
        assert_eq!(
            contacts.lapsed(&all, &alive, takeover + bound, bound),
            ["gone"]
        );
    }

    #[test]
    fn a_consumer_being_discharged_is_not_served_until_the_outcome_is_known() {
        let start = Instant::now();
        let mut contacts = ConsumerContacts::new(start);
        contacts.observe_term(1, start);
        let later = start + Duration::from_secs(90);
        let all = consumers(&["gone"]);
        let lapsed = contacts.lapsed(&all, &HashSet::new(), later, Duration::from_secs(80));
        assert_eq!(lapsed, ["gone"]);
        contacts.begin_discharge("gone");
        assert!(!contacts.record("gone", later));
        assert!(
            contacts
                .lapsed(&all, &HashSet::new(), later, Duration::from_secs(80))
                .is_empty(),
            "a discharge in flight isn't started twice"
        );
        contacts.finish_discharge("gone");
        assert!(contacts.record("gone", later));
        assert!(
            contacts
                .lapsed(&all, &HashSet::new(), later, Duration::from_secs(80))
                .is_empty()
        );
    }
}
