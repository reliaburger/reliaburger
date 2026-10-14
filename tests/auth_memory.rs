//! #945: what checking an API token costs the heap of the node that serves
//! the API.
//!
//! Every request with a bearer token runs Argon2id against the stored hashes,
//! and Argon2id's working memory is 19 MiB at the default cost. In the 0.2.0
//! candidate 4 soak, node 1's Bun moved up and down in steps of exactly
//! 19,456 KiB and sat near 1 GB on a 2 GiB VM: glibc handed each 19 MiB
//! block out of one of its per-thread arenas and kept it there after the
//! check, one or more per arena.
//!
//! Argon2's working memory now comes from a private anonymous mapping that is
//! unmapped as soon as the hash is done, so it never reaches the heap. This
//! binary counts every heap byte through its own global allocator, so the
//! numbers are what the check allocates, not what the allocator keeps mapped.
//!
//! The counters are process-wide, so each test needs its own process, which
//! is how nextest runs them (`make test`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use reliaburger::sesame::auth::authenticate;
use reliaburger::sesame::token::{create_token, validate_token};
use reliaburger::sesame::types::{ApiRole, TokenScope};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to the system allocator with the caller's own
// layout and pointer; the counters are bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged from our caller, who upholds `alloc`'s contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `alloc` above with this same layout.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The heap one Argon2 check may use: the parsed hash, the salt, the output
/// and a few strings. Argon2's own 19 MiB is far above it.
const HEAP_CEILING: usize = 1024 * 1024;

/// Run `work` and return the most heap it had live at once, above what was
/// live when it started.
fn peak_heap_of<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let result = work();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    (result, peak)
}

#[test]
fn checking_a_bearer_keeps_argon2s_working_memory_off_the_heap() {
    // Three stored tokens and the bearer is the last, so the check runs
    // Argon2 three times, as the soak's API node did for each request.
    let stored: Vec<_> = ["operator", "harness", "jobs"]
        .iter()
        .map(|name| create_token(name, ApiRole::Admin, TokenScope::default(), None).unwrap())
        .collect();
    let bearer = stored[2].plaintext.clone();
    let tokens: Vec<_> = stored.into_iter().map(|created| created.token).collect();

    let (context, peak) = peak_heap_of(|| authenticate(&bearer, &tokens));

    assert_eq!(context.unwrap().token_name, "jobs");
    assert!(
        peak < HEAP_CEILING,
        "checking a bearer had {peak} heap bytes live at once; Argon2's working memory must not come from the heap"
    );
}

#[test]
fn a_wrong_secret_is_refused_without_argon2_memory_on_the_heap() {
    let stored = create_token("operator", ApiRole::Admin, TokenScope::default(), None).unwrap();
    let wrong = format!("rbrg_{}", "0".repeat(64));

    let (result, peak) = peak_heap_of(|| validate_token(&wrong, &stored.token));

    assert!(result.is_err());
    assert!(
        peak < HEAP_CEILING,
        "refusing a wrong secret had {peak} heap bytes live at once"
    );
}

#[test]
fn minting_a_token_keeps_argon2s_working_memory_off_the_heap() {
    let (created, peak) =
        peak_heap_of(|| create_token("operator", ApiRole::Admin, TokenScope::default(), None));

    let created = created.unwrap();
    assert!(validate_token(&created.plaintext, &created.token).is_ok());
    assert!(
        peak < HEAP_CEILING,
        "minting a token had {peak} heap bytes live at once"
    );
}
