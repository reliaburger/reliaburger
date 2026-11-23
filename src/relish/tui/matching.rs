//! The `/` search in the log and metric views: which lines match, ignoring
//! case, and where, so a view shows only those lines with each match
//! highlighted.
//!
//! Matching works a character at a time rather than on a lowercased copy of
//! the line, because lowercasing can change a string's length in bytes
//! (`İ` becomes two characters), and the highlight needs byte offsets into
//! the line as it is.

use std::ops::Range;

use ratatui::text::Span;

use super::theme;

/// Byte ranges of every match of `needle` in `text`, ignoring case, left to
/// right and not overlapping. An empty needle matches nowhere.
pub fn match_ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = needle.chars().collect();
    let mut ranges = Vec::new();
    if needle.is_empty() {
        return ranges;
    }
    let mut start = 0;
    while let Some(rest) = text.get(start..).filter(|rest| !rest.is_empty()) {
        match match_length(rest, &needle) {
            Some(length) => {
                ranges.push(start..start + length);
                start += length;
            }
            None => start += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    ranges
}

/// Whether `text` contains `needle`, ignoring case. Everything contains the
/// empty needle, so a search box that's open but empty hides nothing.
pub fn contains(text: &str, needle: &str) -> bool {
    needle.is_empty() || !match_ranges(text, needle).is_empty()
}

/// `text` split into spans, with each match of `needle` in red and bold.
///
/// Red and bold rather than an RGB colour, so the highlight still shows on a
/// 16-colour console such as the appliance's.
pub fn highlight(text: &str, needle: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut end_of_last = 0;
    for range in match_ranges(text, needle) {
        if range.start > end_of_last {
            spans.push(Span::raw(text[end_of_last..range.start].to_string()));
        }
        spans.push(Span::styled(
            text[range.clone()].to_string(),
            theme::search_match(),
        ));
        end_of_last = range.end;
    }
    if end_of_last < text.len() || spans.is_empty() {
        spans.push(Span::raw(text[end_of_last..].to_string()));
    }
    spans
}

/// The length in bytes of `needle` matched at the very start of `text`, or
/// `None` when it doesn't start there.
fn match_length(text: &str, needle: &[char]) -> Option<usize> {
    let mut characters = text.char_indices();
    for wanted in needle {
        let (_, found) = characters.next()?;
        if !found.to_lowercase().eq(wanted.to_lowercase()) {
            return None;
        }
    }
    Some(characters.next().map_or(text.len(), |(index, _)| index))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The highlighted spans' text, each match in brackets.
    fn marked(text: &str, needle: &str) -> String {
        highlight(text, needle)
            .iter()
            .map(|span| {
                if span.style == theme::search_match() {
                    format!("[{}]", span.content)
                } else {
                    span.content.to_string()
                }
            })
            .collect()
    }

    #[test]
    fn several_matches_in_one_line_are_each_highlighted() {
        assert_eq!(
            marked("error: disk error, retry error later", "error"),
            "[error]: disk [error], retry [error] later"
        );
    }

    #[test]
    fn a_match_at_either_end_has_no_empty_span_beside_it() {
        let spans = highlight("timeout waiting for timeout", "timeout");
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!(
            marked("timeout waiting for timeout", "timeout"),
            "[timeout] waiting for [timeout]"
        );
        assert_eq!(marked("timeout", "timeout"), "[timeout]");
    }

    #[test]
    fn a_match_in_the_middle_splits_the_line_in_three() {
        assert_eq!(
            marked("GET /health 200 in 3ms", "health"),
            "GET /[health] 200 in 3ms"
        );
    }

    #[test]
    fn matching_ignores_case_but_the_highlight_keeps_the_lines_own() {
        assert_eq!(marked("Error and ERROR", "error"), "[Error] and [ERROR]");
        assert!(contains("Connection REFUSED", "refused"));
    }

    #[test]
    fn no_match_and_an_empty_needle_leave_the_line_alone() {
        assert_eq!(marked("all quiet", "panic"), "all quiet");
        assert_eq!(highlight("all quiet", "").len(), 1);
        assert!(!contains("all quiet", "panic"));
        assert!(contains("all quiet", ""));
        assert_eq!(highlight("", "x").len(), 1);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(match_ranges("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn multibyte_text_is_split_on_character_boundaries() {
        assert_eq!(marked("café — Café", "CAFÉ"), "[café] — [Café]");
        assert_eq!(marked("→ ready ←", "ready"), "→ [ready] ←");
    }
}
