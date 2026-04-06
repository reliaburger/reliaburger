//! JSON log detection and filtering.
//!
//! Auto-detects structured JSON logs by examining the first N lines.
//! Provides field-level filtering for structured log queries.

/// Check if the given lines are JSON (each line parses as valid JSON object).
///
/// Returns `true` if at least `threshold` of the first `max_lines` parse
/// as JSON objects. Returns `false` for empty input.
pub fn detect_json(lines: &[&str], max_lines: usize, threshold: f64) -> bool {
    let sample: Vec<&&str> = lines.iter().take(max_lines).collect();
    if sample.is_empty() {
        return false;
    }

    let json_count = sample
        .iter()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|v| v.is_object())
        })
        .count();

    (json_count as f64 / sample.len() as f64) >= threshold
}

/// Check if a JSON log line contains a field with the given value.
///
/// Returns `true` if the line parses as JSON and has `key` equal to
/// `value` (string comparison). Returns `false` for non-JSON lines.
pub fn filter_json_field(line: &str, key: &str, value: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    parsed
        .get(key)
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_json_valid_lines() {
        let lines = vec![
            r#"{"level":"info","msg":"starting"}"#,
            r#"{"level":"debug","msg":"loaded config"}"#,
        ];
        assert!(detect_json(
            &lines.iter().map(|s| *s).collect::<Vec<_>>(),
            10,
            0.8
        ));
    }

    #[test]
    fn detect_json_plain_text() {
        let lines = vec!["2024-01-01 INFO starting up", "2024-01-01 DEBUG loading"];
        assert!(!detect_json(
            &lines.iter().map(|s| *s).collect::<Vec<_>>(),
            10,
            0.8
        ));
    }

    #[test]
    fn detect_json_empty_returns_false() {
        let lines: Vec<&str> = vec![];
        assert!(!detect_json(&lines, 10, 0.8));
    }

    #[test]
    fn detect_json_mixed_lines() {
        let lines = vec![
            r#"{"level":"info"}"#,
            "plain text line",
            r#"{"level":"debug"}"#,
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_ref()).collect();
        // 2/3 = 0.67 < 0.8 threshold
        assert!(!detect_json(&refs, 10, 0.8));
        // but passes at 0.5 threshold
        assert!(detect_json(&refs, 10, 0.5));
    }

    #[test]
    fn filter_json_field_matches() {
        let line = r#"{"level":"error","msg":"failed"}"#;
        assert!(filter_json_field(line, "level", "error"));
    }

    #[test]
    fn filter_json_field_no_match() {
        let line = r#"{"level":"info","msg":"ok"}"#;
        assert!(!filter_json_field(line, "level", "error"));
    }

    #[test]
    fn filter_json_field_non_json() {
        assert!(!filter_json_field("plain text", "level", "error"));
    }

    #[test]
    fn filter_json_field_missing_key() {
        let line = r#"{"msg":"hello"}"#;
        assert!(!filter_json_field(line, "level", "info"));
    }
}
