//! `relish source` — the source code the binary was built from, embedded
//! and fuzzy-searchable in the terminal.
//!
//! The whole `src/` tree is compiled in with `rust-embed` (compressed in
//! release builds) and browsed through the shared [`reader`]: file list on
//! the left, file content on the right, `/` for fuzzy search across paths
//! and text. `relish source ebpf` opens with the query pre-seeded.

use rust_embed::Embed;

use super::RelishError;
use super::reader::{self, Document};

/// The `src/` tree, compiled in at build time.
#[derive(Embed)]
#[folder = "src/"]
pub struct SourceAssets;

/// Every embedded source path, sorted.
pub fn paths() -> Vec<String> {
    let mut paths: Vec<String> = SourceAssets::iter().map(|path| path.to_string()).collect();
    paths.sort();
    paths
}

/// The embedded sources as reader documents. Each document starts with a
/// styled path header line so a fuzzy query matches file paths as well as
/// file contents.
pub fn documents() -> Vec<Document> {
    use ratatui::text::{Line, Span};

    use super::tui::theme;

    paths()
        .into_iter()
        .filter_map(|path| {
            let asset = SourceAssets::get(&path)?;
            let content = String::from_utf8_lossy(asset.data.as_ref());
            let mut lines = Vec::new();
            lines.push(Line::from(Span::styled(path.clone(), theme::title())));
            lines.push(Line::default());
            lines.extend(content.lines().map(|line| Line::raw(line.to_string())));
            Some(Document { title: path, lines })
        })
        .collect()
}

/// `relish source [query]` — open the reader, optionally mid-search.
pub async fn run(query: Option<String>) -> Result<(), RelishError> {
    reader::run("files", documents(), query).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::reader::{fuzzy_rank, line_text};

    #[test]
    fn embedded_tree_contains_known_sources() {
        let paths = paths();
        assert!(
            paths.len() > 200,
            "expected the full src tree, got {}",
            paths.len()
        );
        assert!(paths.contains(&"bin/relish.rs".to_string()));
        assert!(paths.contains(&"relish/source.rs".to_string()));
        // Sorted for a stable list pane.
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn fuzzy_ebpf_finds_the_ebpf_sources() {
        let paths = paths();
        let ranked = fuzzy_rank("ebpf", &paths);
        assert!(!ranked.is_empty());
        let top: Vec<&str> = ranked.iter().take(5).map(|&i| paths[i].as_str()).collect();
        assert!(
            top.iter().any(|path| path.contains("ebpf")),
            "expected an ebpf path among the top hits, got {top:?}"
        );
    }

    #[test]
    fn documents_start_with_their_path_and_carry_the_content() {
        let documents = documents();
        assert_eq!(documents.len(), paths().len());
        let this_file = documents
            .iter()
            .find(|doc| doc.title == "relish/source.rs")
            .expect("this file is embedded");
        assert_eq!(line_text(&this_file.lines[0]), "relish/source.rs");
        assert!(
            this_file
                .lines
                .iter()
                .any(|line| line_text(line).contains("fuzzy-searchable")),
            "file content is present"
        );
    }
}
