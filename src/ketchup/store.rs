//! Log storage: append-only files with day rotation and queries.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::index::{INDEX_INTERVAL, SparseIndex};
use super::json;
use super::types::{KetchupError, LogEntry, LogQuery, LogStream};

/// Manages log storage for all apps.
pub struct KetchupStore {
    base_dir: PathBuf,
}

impl KetchupStore {
    /// Create a new store rooted at `base_dir`.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Directory for an app's logs.
    fn app_dir(&self, app: &str, namespace: &str) -> PathBuf {
        self.base_dir.join(namespace).join(app)
    }

    /// Log file path for a given date.
    fn log_path(&self, app: &str, namespace: &str, date: &str) -> PathBuf {
        self.app_dir(app, namespace).join(format!("{date}.log"))
    }

    /// Index file path for a given date.
    fn index_path(&self, app: &str, namespace: &str, date: &str) -> PathBuf {
        self.app_dir(app, namespace).join(format!("{date}.idx"))
    }

    /// Current date string (YYYY-MM-DD).
    fn today() -> String {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = now / 86400;
        // Simple conversion: days since epoch to YYYY-MM-DD
        let y = 1970 + (days * 400 / 146097) as u32;
        format!("{y}-{:02}-{:02}", (days % 365 / 30) + 1, (days % 30) + 1)
    }

    /// Append a log line for an app.
    pub fn append(
        &self,
        app: &str,
        namespace: &str,
        stream: LogStream,
        line: &str,
    ) -> Result<(), KetchupError> {
        let date = Self::today();
        let log_path = self.log_path(app, namespace, &date);

        std::fs::create_dir_all(log_path.parent().unwrap())?;

        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let stream_prefix = match stream {
            LogStream::Stdout => "O",
            LogStream::Stderr => "E",
        };

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        let record = format!("{timestamp} {stream_prefix} {line}\n");
        let offset_before = file.metadata()?.len();
        file.write_all(record.as_bytes())?;

        // Update sparse index if we crossed an interval boundary
        let offset_after = offset_before + record.len() as u64;
        if offset_before / INDEX_INTERVAL != offset_after / INDEX_INTERVAL {
            let idx_path = self.index_path(app, namespace, &date);
            let mut index = if idx_path.exists() {
                SparseIndex::read_from(&idx_path).unwrap_or_default()
            } else {
                SparseIndex::new()
            };
            index.add(offset_before, timestamp);
            index.write_to(&idx_path)?;
        }

        Ok(())
    }

    /// Query logs for an app.
    pub fn query(&self, q: &LogQuery) -> Result<Vec<LogEntry>, KetchupError> {
        let app_dir = self.app_dir(&q.app, &q.namespace);
        if !app_dir.exists() {
            return Ok(Vec::new());
        }

        let mut all_entries = Vec::new();

        // Read all .log files in the app directory
        let mut log_files: Vec<PathBuf> = std::fs::read_dir(&app_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
            .map(|e| e.path())
            .collect();
        log_files.sort();

        for log_file in &log_files {
            let content = std::fs::read_to_string(log_file)?;

            for line in content.lines() {
                let Some(entry) = parse_log_line(line) else {
                    continue;
                };

                // Time range filter
                if let Some(start) = q.start
                    && entry.timestamp < start
                {
                    continue;
                }
                if let Some(end) = q.end
                    && entry.timestamp > end
                {
                    continue;
                }

                // Grep filter
                if let Some(ref pattern) = q.grep
                    && !entry.line.contains(pattern.as_str())
                {
                    continue;
                }

                // JSON field filter
                if let Some((ref key, ref value)) = q.json_field
                    && !json::filter_json_field(&entry.line, key, value)
                {
                    continue;
                }

                all_entries.push(entry);
            }
        }

        // Apply tail
        if let Some(tail) = q.tail {
            let skip = all_entries.len().saturating_sub(tail);
            all_entries = all_entries.into_iter().skip(skip).collect();
        }

        Ok(all_entries)
    }

    /// Delete log files older than `retention_days`.
    pub fn enforce_retention(&self, retention_days: u32) -> Result<usize, KetchupError> {
        let cutoff = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            - (retention_days as u64 * 86400);

        let mut deleted = 0;

        if !self.base_dir.exists() {
            return Ok(0);
        }

        // Walk namespace/app directories
        for ns_entry in std::fs::read_dir(&self.base_dir)?.flatten() {
            if !ns_entry.file_type()?.is_dir() {
                continue;
            }
            for app_entry in std::fs::read_dir(ns_entry.path())?.flatten() {
                if !app_entry.file_type()?.is_dir() {
                    continue;
                }
                for file_entry in std::fs::read_dir(app_entry.path())?.flatten() {
                    let path = file_entry.path();
                    if let Ok(meta) = std::fs::metadata(&path)
                        && let Ok(modified) = meta.modified()
                    {
                        let mod_secs = modified
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        if mod_secs < cutoff {
                            let _ = std::fs::remove_file(&path);
                            deleted += 1;
                        }
                    }
                }
            }
        }

        Ok(deleted)
    }

    /// The base directory.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

/// Parse a log line in the format: `{timestamp} {stream} {content}`.
fn parse_log_line(line: &str) -> Option<LogEntry> {
    let mut parts = line.splitn(3, ' ');
    let timestamp: u64 = parts.next()?.parse().ok()?;
    let stream_char = parts.next()?;
    let content = parts.next().unwrap_or("");

    let stream = match stream_char {
        "O" => LogStream::Stdout,
        "E" => LogStream::Stderr,
        _ => return None,
    };

    Some(LogEntry {
        timestamp,
        stream,
        line: content.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (KetchupStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = KetchupStore::new(dir.path());
        (store, dir)
    }

    #[test]
    fn append_and_query() {
        let (store, _dir) = test_store();
        store
            .append("web", "default", LogStream::Stdout, "hello world")
            .unwrap();

        let entries = store
            .query(&LogQuery {
                app: "web".to_string(),
                namespace: "default".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].line, "hello world");
        assert_eq!(entries[0].stream, LogStream::Stdout);
    }

    #[test]
    fn append_multiple_lines() {
        let (store, _dir) = test_store();
        store
            .append("web", "default", LogStream::Stdout, "line 1")
            .unwrap();
        store
            .append("web", "default", LogStream::Stderr, "line 2")
            .unwrap();
        store
            .append("web", "default", LogStream::Stdout, "line 3")
            .unwrap();

        let entries = store
            .query(&LogQuery {
                app: "web".to_string(),
                namespace: "default".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn query_with_grep() {
        let (store, _dir) = test_store();
        store
            .append("web", "default", LogStream::Stdout, "INFO starting")
            .unwrap();
        store
            .append("web", "default", LogStream::Stderr, "ERROR failed")
            .unwrap();
        store
            .append("web", "default", LogStream::Stdout, "INFO ready")
            .unwrap();

        let entries = store
            .query(&LogQuery {
                app: "web".to_string(),
                namespace: "default".to_string(),
                grep: Some("ERROR".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].line.contains("ERROR"));
    }

    #[test]
    fn query_with_tail() {
        let (store, _dir) = test_store();
        for i in 0..10 {
            store
                .append("web", "default", LogStream::Stdout, &format!("line {i}"))
                .unwrap();
        }

        let entries = store
            .query(&LogQuery {
                app: "web".to_string(),
                namespace: "default".to_string(),
                tail: Some(3),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[2].line.contains("line 9"));
    }

    #[test]
    fn query_empty_store() {
        let (store, _dir) = test_store();
        let entries = store
            .query(&LogQuery {
                app: "nonexistent".to_string(),
                namespace: "default".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn query_with_json_field_filter() {
        let (store, _dir) = test_store();
        store
            .append(
                "api",
                "default",
                LogStream::Stdout,
                r#"{"level":"info","msg":"ok"}"#,
            )
            .unwrap();
        store
            .append(
                "api",
                "default",
                LogStream::Stdout,
                r#"{"level":"error","msg":"fail"}"#,
            )
            .unwrap();

        let entries = store
            .query(&LogQuery {
                app: "api".to_string(),
                namespace: "default".to_string(),
                json_field: Some(("level".to_string(), "error".to_string())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].line.contains("fail"));
    }

    #[test]
    fn stdout_and_stderr_distinguished() {
        let (store, _dir) = test_store();
        store
            .append("web", "default", LogStream::Stdout, "out")
            .unwrap();
        store
            .append("web", "default", LogStream::Stderr, "err")
            .unwrap();

        let entries = store
            .query(&LogQuery {
                app: "web".to_string(),
                namespace: "default".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries[0].stream, LogStream::Stdout);
        assert_eq!(entries[1].stream, LogStream::Stderr);
    }

    #[test]
    fn parse_log_line_valid() {
        let entry = parse_log_line("1000 O hello world").unwrap();
        assert_eq!(entry.timestamp, 1000);
        assert_eq!(entry.stream, LogStream::Stdout);
        assert_eq!(entry.line, "hello world");
    }

    #[test]
    fn parse_log_line_stderr() {
        let entry = parse_log_line("2000 E error msg").unwrap();
        assert_eq!(entry.stream, LogStream::Stderr);
    }

    #[test]
    fn parse_log_line_invalid() {
        assert!(parse_log_line("not a valid line").is_none());
        assert!(parse_log_line("").is_none());
    }

    #[test]
    fn different_apps_isolated() {
        let (store, _dir) = test_store();
        store
            .append("web", "default", LogStream::Stdout, "web log")
            .unwrap();
        store
            .append("api", "default", LogStream::Stdout, "api log")
            .unwrap();

        let web_entries = store
            .query(&LogQuery {
                app: "web".to_string(),
                namespace: "default".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(web_entries.len(), 1);
        assert!(web_entries[0].line.contains("web"));

        let api_entries = store
            .query(&LogQuery {
                app: "api".to_string(),
                namespace: "default".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(api_entries.len(), 1);
        assert!(api_entries[0].line.contains("api"));
    }
}
