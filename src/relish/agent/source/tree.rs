//! `relish agent source tree`: what the bundle holds, with sizes.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;

use super::{Answer, Bundle, normalise_path, not_found, quote, text_of};
use crate::relish::agent::envelope::AgentError;

/// Above this many entries, `next` suggests a `--depth`.
const MANY_ENTRIES: usize = 100;

/// `data` for `relish agent source tree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct TreeData {
    /// The prefix the listing is under, or `null` for the whole bundle.
    pub prefix: Option<String>,
    /// The `--depth` it was rolled up to, or `null` for every file.
    pub depth: Option<u32>,
    /// How many files are under the prefix.
    pub files: usize,
    /// Their total size in bytes.
    pub bytes: u64,
    /// Files and rolled-up directories, sorted by path.
    pub entries: Vec<TreeEntry>,
}

/// One file, or one directory `--depth` rolled up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct TreeEntry {
    /// Repository-relative path; a directory's ends with `/`.
    pub path: String,
    /// File or directory.
    pub kind: EntryKind,
    /// Size in bytes; a directory's is its files' total.
    pub bytes: u64,
    /// Line count, or `null` for a binary file; a directory's counts its
    /// text files.
    pub lines: Option<u64>,
    /// How many files a directory holds; `null` for a file.
    pub files: Option<usize>,
}

/// What a [`TreeEntry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A file.
    File,
    /// A directory whose contents `--depth` rolled up.
    Dir,
}

/// The number of lines in `text`, counting a last line without a newline.
pub(super) fn line_count(text: &str) -> u64 {
    text.lines().count() as u64
}

/// Whether `path` is `prefix` or under it. An empty prefix holds everything.
fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(super) fn tree(
    bundle: &Bundle,
    prefix: Option<&str>,
    depth: Option<u32>,
) -> Result<Answer<TreeData>, AgentError> {
    let wanted = prefix.map(normalise_path).unwrap_or("");
    let files: Vec<_> = bundle
        .files
        .iter()
        .filter(|file| under(&file.path, wanted))
        .collect();
    if files.is_empty() {
        return Err(not_found(bundle, wanted));
    }

    // Directories collect in a map keyed by path, so each appears once and
    // the entries come out sorted with the files.
    let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for file in &files {
        let bytes = file.contents.len() as u64;
        let lines = text_of(file).map(line_count);
        match depth.and_then(|depth| rolled_up(&file.path, wanted, depth)) {
            Some(dir) => {
                let entry = entries.entry(dir.clone()).or_insert(TreeEntry {
                    path: dir,
                    kind: EntryKind::Dir,
                    bytes: 0,
                    lines: Some(0),
                    files: Some(0),
                });
                entry.bytes += bytes;
                entry.lines = entry.lines.map(|sum| sum + lines.unwrap_or(0));
                entry.files = entry.files.map(|count| count + 1);
            }
            None => {
                entries.insert(
                    file.path.clone(),
                    TreeEntry {
                        path: file.path.clone(),
                        kind: EntryKind::File,
                        bytes,
                        lines,
                        files: None,
                    },
                );
            }
        }
    }

    let data = TreeData {
        prefix: prefix.map(|_| wanted.to_string()),
        depth,
        files: files.len(),
        bytes: files.iter().map(|file| file.contents.len() as u64).sum(),
        entries: entries.into_values().collect(),
    };
    let mut answer = Answer::new(data);
    let target = if wanted.is_empty() {
        String::new()
    } else {
        format!(" {}", quote(wanted))
    };
    if depth.is_none() && answer.data.entries.len() > MANY_ENTRIES {
        answer = answer.with_next(
            format!("relish agent source tree{target} --depth 1"),
            "the same listing rolled up one level deep",
            true,
        );
    }
    if let Some(dir) = answer
        .data
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Dir)
        // The biggest directory is usually where the code is; the first one
        // alphabetically is often assets.
        .max_by_key(|entry| entry.bytes)
    {
        let dir = dir.path.trim_end_matches('/').to_string();
        answer = answer.with_next(
            format!("relish agent source tree {}", quote(&dir)),
            "the files in the largest directory",
            true,
        );
    } else if let Some(file) = answer.data.entries.first() {
        let path = file.path.clone();
        answer = answer.with_next(
            format!("relish agent source show {}", quote(&path)),
            "the first file",
            true,
        );
    }
    Ok(answer)
}

/// The directory (ending in `/`) `path` rolls up into at `depth` levels
/// below `prefix`, or `None` when the file itself is shallow enough.
fn rolled_up(path: &str, prefix: &str, depth: u32) -> Option<String> {
    let relative = if prefix.is_empty() {
        path
    } else {
        path.strip_prefix(prefix)?.trim_start_matches('/')
    };
    let parts: Vec<&str> = relative.split('/').collect();
    let depth = depth as usize;
    if parts.len() <= depth {
        return None;
    }
    let kept = parts[..depth].join("/");
    Some(if prefix.is_empty() {
        format!("{kept}/")
    } else {
        format!("{prefix}/{kept}/")
    })
}

#[cfg(test)]
mod tests {
    use super::super::SourceCommand;
    use super::super::fixture::*;
    use super::super::nodes::ClusterSource;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;

    fn run(prefix: Option<&str>, depth: Option<u32>) -> (serde_json::Value, u8) {
        answer_json(
            &bundle(),
            &SourceCommand::Tree {
                prefix: prefix.map(str::to_string),
                depth,
            },
            &ClusterSource::Offline,
            DEFAULT_MAX_BYTES,
        )
    }

    #[test]
    fn snapshot_of_the_whole_tree() {
        let (value, exit) = run(None, None);
        assert_eq!(exit, 0);
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn snapshot_of_a_tree_one_level_deep() {
        let (value, _) = run(None, Some(1));
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn a_prefix_lists_only_what_is_under_it() {
        let (value, _) = run(Some("./src/"), None);
        assert_eq!(value["data"]["prefix"], "src");
        let paths: Vec<&str> = value["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["src/bin/tool.rs", "src/client.rs", "src/lib.rs"]);
    }

    #[test]
    fn depth_counts_from_the_prefix() {
        let (value, _) = run(Some("src"), Some(1));
        let paths: Vec<&str> = value["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["src/bin/", "src/client.rs", "src/lib.rs"]);
    }

    #[test]
    fn a_prefix_must_not_match_half_a_name() {
        let (value, exit) = run(Some("sr"), None);
        assert_eq!(exit, 5);
        assert_eq!(value["error"]["code"], "not_found");
    }

    #[test]
    fn line_counts_include_a_last_line_without_a_newline() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("one"), 1);
        assert_eq!(line_count("one\ntwo\n"), 2);
        assert_eq!(line_count("one\r\ntwo"), 2);
    }
}
