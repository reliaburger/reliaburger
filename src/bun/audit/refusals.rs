//! Rate-limiting the audit records of refused requests (#798).
//!
//! A request the API refuses (a bad token, a role or `[permission]` block, a
//! scoped token reaching outside its scope) changes nothing, but it is
//! exactly what an operator wants to see after an incident: who tried what,
//! from where. So the audit log records refusals too.
//!
//! Refusals, unlike real actions, cost the caller nothing. A script with a
//! wrong token can make thousands a second, and the log is bounded
//! (`[storage] audit_max_bytes`): stored one by one, a flood would push every
//! real record out of it. The [`RefusalLimiter`] stops that. It keeps a fixed
//! window per source address and principal: the first
//! [`REFUSALS_PER_WINDOW`] refusals in a window are recorded, the rest are
//! only counted, and when the window closes one summary record says how many
//! were suppressed. All sources together are capped at
//! [`REFUSALS_PER_WINDOW_ALL_SOURCES`] records a window too, so a flood from
//! many addresses can't fill the log either.
//!
//! The limiter's memory is bounded: it tracks at most
//! [`MAX_REFUSAL_SOURCES`] sources. Refusals from further sources, while the
//! table is full, are counted together under [`RefusalSource::Overflow`].

use std::collections::HashMap;
use std::net::IpAddr;

/// Refusals recorded one by one per source and principal in a window.
pub const REFUSALS_PER_WINDOW: u32 = 5;

/// Refusals recorded one by one in a window, from all sources together.
pub const REFUSALS_PER_WINDOW_ALL_SOURCES: u32 = 100;

/// Length of a rate-limiting window, in seconds.
pub const REFUSAL_WINDOW_SECONDS: u64 = 60;

/// The most sources the limiter tracks at once, besides
/// [`RefusalSource::Overflow`].
pub const MAX_REFUSAL_SOURCES: usize = 256;

/// Whom a refusal is counted against.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum RefusalSource {
    /// One client: its IP address (without the port, which changes with
    /// every connection) and the principal it presented.
    Caller {
        /// The client's address, when the server knows it.
        address: Option<IpAddr>,
        /// The authenticated principal, or `anonymous`.
        principal: String,
    },
    /// Every source that arrived while the table was full.
    Overflow,
}

/// One closed window in which refusals from a source were suppressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressedRefusals {
    /// Whose refusals were suppressed.
    pub source: RefusalSource,
    /// How many were counted but not recorded.
    pub suppressed: u64,
    /// When the window opened, as a Unix timestamp in seconds.
    pub window_start: u64,
}

/// What to do with one refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalVerdict {
    /// Record this refusal on its own.
    pub record: bool,
    /// Summaries of windows that closed, to record as well.
    pub summaries: Vec<SuppressedRefusals>,
}

/// One source's current window.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: u64,
    recorded: u32,
    suppressed: u64,
}

impl Window {
    fn open(now: u64) -> Self {
        Self {
            start: now,
            recorded: 0,
            suppressed: 0,
        }
    }

    /// A clock that steps backwards keeps the window open rather than
    /// underflowing.
    fn closed_at(&self, now: u64) -> bool {
        now.saturating_sub(self.start) >= REFUSAL_WINDOW_SECONDS
    }
}

/// Decides which refusals are recorded one by one and which are only
/// counted. Pure and synchronous: the caller passes the time in, so tests
/// don't wait for a window to pass.
#[derive(Debug)]
pub struct RefusalLimiter {
    sources: HashMap<RefusalSource, Window>,
    all_sources: Window,
}

impl Default for RefusalLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RefusalLimiter {
    /// An empty limiter.
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            all_sources: Window::open(0),
        }
    }

    /// Count one refusal from `source` at `now` (Unix seconds) and say
    /// whether to record it, plus the summaries of any windows that have
    /// closed since the last call.
    pub fn admit(&mut self, source: RefusalSource, now: u64) -> RefusalVerdict {
        let summaries = self.sweep(now);
        let key = if self.sources.contains_key(&source) || self.sources.len() < MAX_REFUSAL_SOURCES
        {
            source
        } else {
            RefusalSource::Overflow
        };
        if self.all_sources.closed_at(now) {
            self.all_sources = Window::open(now);
        }
        let window = self.sources.entry(key).or_insert_with(|| Window::open(now));
        let record = window.recorded < REFUSALS_PER_WINDOW
            && self.all_sources.recorded < REFUSALS_PER_WINDOW_ALL_SOURCES;
        if record {
            window.recorded += 1;
            self.all_sources.recorded += 1;
        } else {
            window.suppressed += 1;
        }
        RefusalVerdict { record, summaries }
    }

    /// Close every window that has run its length at `now`, forget its
    /// source, and return a summary for each that suppressed something.
    pub fn sweep(&mut self, now: u64) -> Vec<SuppressedRefusals> {
        let mut summaries = Vec::new();
        self.sources.retain(|source, window| {
            if !window.closed_at(now) {
                return true;
            }
            if window.suppressed > 0 {
                summaries.push(SuppressedRefusals {
                    source: source.clone(),
                    suppressed: window.suppressed,
                    window_start: window.start,
                });
            }
            false
        });
        summaries.sort_by_key(|summary| summary.window_start);
        summaries
    }

    /// How many sources the limiter is tracking.
    pub fn tracked_sources(&self) -> usize {
        self.sources.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(address: &str, principal: &str) -> RefusalSource {
        RefusalSource::Caller {
            address: Some(address.parse().unwrap()),
            principal: principal.to_string(),
        }
    }

    const T0: u64 = 1_800_000_000;

    #[test]
    fn a_burst_of_refusals_is_summarised_not_stored_one_by_one() {
        let mut limiter = RefusalLimiter::new();
        let source = caller("203.0.113.9", "anonymous");
        let recorded = (0..50)
            .filter(|_| limiter.admit(source.clone(), T0).record)
            .count();
        assert_eq!(recorded, REFUSALS_PER_WINDOW as usize);

        // The next refusal after the window closes carries the summary and
        // is itself recorded, in a fresh window.
        let verdict = limiter.admit(source.clone(), T0 + REFUSAL_WINDOW_SECONDS);
        assert!(verdict.record);
        assert_eq!(
            verdict.summaries,
            vec![SuppressedRefusals {
                source,
                suppressed: 50 - u64::from(REFUSALS_PER_WINDOW),
                window_start: T0,
            }]
        );
    }

    #[test]
    fn suppressed_refusals_are_summarised_when_the_window_closes() {
        let mut limiter = RefusalLimiter::new();
        let source = caller("203.0.113.9", "token:alice");
        for _ in 0..8 {
            limiter.admit(source.clone(), T0);
        }
        assert!(limiter.sweep(T0 + REFUSAL_WINDOW_SECONDS - 1).is_empty());
        let summaries = limiter.sweep(T0 + REFUSAL_WINDOW_SECONDS);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].suppressed, 3);
        assert_eq!(limiter.tracked_sources(), 0, "a closed window is forgotten");
        assert!(
            limiter.sweep(T0 + 10 * REFUSAL_WINDOW_SECONDS).is_empty(),
            "a summary is made once"
        );
    }

    #[test]
    fn a_window_that_suppressed_nothing_leaves_no_summary() {
        let mut limiter = RefusalLimiter::new();
        limiter.admit(caller("203.0.113.9", "anonymous"), T0);
        assert!(limiter.sweep(T0 + REFUSAL_WINDOW_SECONDS).is_empty());
    }

    #[test]
    fn sources_and_principals_are_limited_separately() {
        let mut limiter = RefusalLimiter::new();
        for _ in 0..REFUSALS_PER_WINDOW {
            assert!(limiter.admit(caller("203.0.113.9", "anonymous"), T0).record);
        }
        assert!(!limiter.admit(caller("203.0.113.9", "anonymous"), T0).record);
        assert!(limiter.admit(caller("203.0.113.9", "token:bob"), T0).record);
        assert!(
            limiter
                .admit(caller("198.51.100.1", "anonymous"), T0)
                .record
        );
    }

    #[test]
    fn tracked_sources_stay_bounded() {
        let mut limiter = RefusalLimiter::new();
        for index in 0..10_000u32 {
            let [a, b, c, d] = index.to_be_bytes();
            let address = std::net::Ipv4Addr::new(10 + a, b, c, d);
            limiter.admit(
                RefusalSource::Caller {
                    address: Some(address.into()),
                    principal: "anonymous".to_string(),
                },
                T0,
            );
        }
        assert!(
            limiter.tracked_sources() <= MAX_REFUSAL_SOURCES + 1,
            "{}",
            limiter.tracked_sources()
        );
        // Every refusal is accounted for: recorded, or counted in a summary.
        let suppressed: u64 = limiter
            .sweep(T0 + REFUSAL_WINDOW_SECONDS)
            .iter()
            .map(|summary| summary.suppressed)
            .sum();
        assert_eq!(
            suppressed,
            10_000 - u64::from(REFUSALS_PER_WINDOW_ALL_SOURCES)
        );
    }

    #[test]
    fn all_sources_together_are_capped_per_window() {
        let mut limiter = RefusalLimiter::new();
        let mut recorded = 0;
        for index in 0..200u8 {
            for _ in 0..REFUSALS_PER_WINDOW {
                let source = caller(&format!("192.0.2.{index}"), "anonymous");
                if limiter.admit(source, T0).record {
                    recorded += 1;
                }
            }
        }
        assert_eq!(recorded, REFUSALS_PER_WINDOW_ALL_SOURCES);
        // The next window records again.
        let later = T0 + REFUSAL_WINDOW_SECONDS;
        assert!(
            limiter
                .admit(caller("192.0.2.1", "anonymous"), later)
                .record
        );
    }

    #[test]
    fn a_clock_stepping_backwards_keeps_the_window_open() {
        let mut limiter = RefusalLimiter::new();
        let source = caller("203.0.113.9", "anonymous");
        for _ in 0..REFUSALS_PER_WINDOW {
            limiter.admit(source.clone(), T0);
        }
        let verdict = limiter.admit(source, T0 - 3600);
        assert!(!verdict.record);
        assert!(verdict.summaries.is_empty());
    }
}
