//! Cross-node log query coordination.
//!
//! When an app runs on multiple nodes, the leader fans out the log
//! query to each node's `/v1/logs` API and merges results by timestamp.

use super::types::{KetchupError, LogEntry, LogQuery};

/// Merge log entries from multiple nodes, sorted by timestamp.
///
/// Deduplicates entries with the same timestamp and line content
/// (which can happen if the same log line is reported by multiple
/// sources).
pub fn merge_log_entries(mut sources: Vec<Vec<LogEntry>>) -> Vec<LogEntry> {
    let mut all: Vec<LogEntry> = sources.drain(..).flatten().collect();
    all.sort_by_key(|e| e.timestamp);

    // Deduplicate by (timestamp, line)
    all.dedup_by(|a, b| a.timestamp == b.timestamp && a.line == b.line);

    all
}

/// Fan out a log query to multiple node URLs, collect results, merge.
///
/// Each `node_url` should be a base URL like `http://10.0.1.5:9117`.
/// The query is sent as query parameters to `GET /v1/logs/{app}/{namespace}`.
pub async fn fan_out_query(
    query: &LogQuery,
    node_urls: &[String],
    client: &reqwest::Client,
    timeout: std::time::Duration,
) -> Result<Vec<LogEntry>, KetchupError> {
    let mut handles = Vec::new();

    for url in node_urls {
        let base = url.clone();
        let app = query.app.clone();
        let namespace = query.namespace.clone();
        let grep = query.grep.clone();
        let tail = query.tail;
        let start = query.start;
        let end = query.end;
        let client = client.clone();

        handles.push(tokio::spawn(async move {
            let mut req_url = format!("{base}/v1/logs/entries/{app}/{namespace}");
            let mut params = Vec::new();
            if let Some(t) = tail {
                params.push(format!("tail={t}"));
            }
            if let Some(ref g) = grep {
                params.push(format!("grep={g}"));
            }
            if let Some(s) = start {
                params.push(format!("start={s}"));
            }
            if let Some(e) = end {
                params.push(format!("end={e}"));
            }
            if !params.is_empty() {
                req_url.push('?');
                req_url.push_str(&params.join("&"));
            }

            let resp = tokio::time::timeout(timeout, client.get(&req_url).send()).await;

            match resp {
                Ok(Ok(r)) if r.status().is_success() => {
                    // Try to parse as JSON array of LogEntry
                    r.json::<Vec<LogEntry>>().await.unwrap_or_default()
                }
                _ => Vec::new(), // Node unreachable or error — return empty
            }
        }));
    }

    let mut sources = Vec::new();
    for handle in handles {
        if let Ok(entries) = handle.await {
            sources.push(entries);
        }
    }

    Ok(merge_log_entries(sources))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ketchup::types::LogStream;

    fn entry(ts: u64, line: &str) -> LogEntry {
        LogEntry {
            timestamp: ts,
            stream: LogStream::Stdout,
            line: line.to_string(),
        }
    }

    #[test]
    fn merge_empty_sources() {
        let result = merge_log_entries(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn merge_single_source() {
        let source = vec![entry(1, "a"), entry(2, "b")];
        let result = merge_log_entries(vec![source]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp, 1);
    }

    #[test]
    fn merge_multiple_sources_sorted() {
        let s1 = vec![entry(1, "a"), entry(3, "c")];
        let s2 = vec![entry(2, "b"), entry(4, "d")];
        let result = merge_log_entries(vec![s1, s2]);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].timestamp, 1);
        assert_eq!(result[1].timestamp, 2);
        assert_eq!(result[2].timestamp, 3);
        assert_eq!(result[3].timestamp, 4);
    }

    #[test]
    fn merge_deduplicates_same_timestamp_and_line() {
        let s1 = vec![entry(1, "same line")];
        let s2 = vec![entry(1, "same line")];
        let result = merge_log_entries(vec![s1, s2]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merge_keeps_different_lines_at_same_timestamp() {
        let s1 = vec![entry(1, "line A")];
        let s2 = vec![entry(1, "line B")];
        let result = merge_log_entries(vec![s1, s2]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn merge_handles_overlapping_ranges() {
        let s1 = vec![entry(1, "a"), entry(2, "b"), entry(3, "c")];
        let s2 = vec![entry(2, "b"), entry(3, "c"), entry(4, "d")];
        let result = merge_log_entries(vec![s1, s2]);
        // 1:a, 2:b (deduped), 3:c (deduped), 4:d
        assert_eq!(result.len(), 4);
    }
}
