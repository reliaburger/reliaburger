//! Fitting a list into the byte budget before the budget has to.
//!
//! [`super::budget`] trims any answer that's too big, but it only knows
//! JSON: it drops array items from the end and cuts strings, then names each
//! cut in a warning. A command that knows its own data can do better. `logs`
//! wants to drop the *oldest* lines, not the newest, and keep them in time
//! order; `manual show` wants to cut Markdown at a line, not mid-word. Both
//! ask [`largest_that_fits`] how many items they can keep, then say in their
//! own `data` what they left out.

use serde::Serialize;

use super::envelope::Envelope;

/// The largest `count` in `0..=total` for which `fits(count)` holds, or
/// `None` when not even `0` fits.
///
/// `fits` must be monotonic: if `n` items fit, so do fewer. It's called
/// about `log2(total)` times, so a caller can rebuild its whole answer in it.
pub fn largest_that_fits(total: usize, fits: impl Fn(usize) -> bool) -> Option<usize> {
    if !fits(0) {
        return None;
    }
    // Invariant: `low` fits; everything above `high` doesn't.
    let (mut low, mut high) = (0, total);
    while low < high {
        // Round up, so `middle > low` and the loop always moves.
        let middle = low + (high - low).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Some(low)
}

/// How many bytes `envelope` takes as `relish agent` prints it: one line of
/// JSON and its newline, as [`super::budget::render`] measures it.
pub fn printed_length<T: Serialize>(envelope: &Envelope<T>) -> usize {
    // Serialising these plain structs can't fail: every map key is a string.
    serde_json::to_string(envelope).map_or(usize::MAX, |json| json.len() + 1)
}

/// `bytes` rounded up to a whole KiB, for a `--max-bytes` hint.
pub fn round_up_to_kib(bytes: usize) -> usize {
    bytes.div_ceil(1024) * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_largest_count_that_fits() {
        for limit in 0..=20 {
            assert_eq!(
                largest_that_fits(20, |count| count <= limit),
                Some(limit),
                "{limit}"
            );
        }
        assert_eq!(largest_that_fits(5, |_| true), Some(5));
        assert_eq!(largest_that_fits(0, |_| true), Some(0));
    }

    #[test]
    fn says_none_when_nothing_fits() {
        assert_eq!(largest_that_fits(10, |_| false), None);
    }

    #[test]
    fn rounds_up_to_whole_kib() {
        assert_eq!(round_up_to_kib(1), 1024);
        assert_eq!(round_up_to_kib(1024), 1024);
        assert_eq!(round_up_to_kib(1025), 2048);
    }
}
