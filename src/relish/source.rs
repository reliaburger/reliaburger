//! `relish source` — the source code the binary was built from, embedded
//! and fuzzy-searchable in the terminal.
//!
//! The files come from the [`source_bundle`](super::source_bundle): the
//! build, test and read set, one zstd tarball that release builds embed.
//! They're browsed through the shared [`reader`]: file list on the left,
//! file content on the right, `/` for fuzzy search across paths and text.
//! `relish source ebpf` opens with the query pre-seeded.

use std::path::Path;

use super::RelishError;
use super::reader::{self, Document};
use super::source_bundle::{self, SourceFile};

/// Every text file in the bundle as a reader document, sorted by path.
/// Each document starts with a styled path header line so a fuzzy query
/// matches file paths as well as file contents. Binary files (test
/// fixtures, mostly) are left out: there's nothing to read.
pub fn documents() -> Result<Vec<Document>, RelishError> {
    Ok(source_bundle::files()?
        .into_iter()
        .filter_map(document)
        .collect())
}

fn document(file: SourceFile) -> Option<Document> {
    use ratatui::text::{Line, Span};

    use super::tui::theme;

    let content = String::from_utf8(file.contents).ok()?;
    if content.contains('\0') {
        return None;
    }
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(file.path.clone(), theme::title())));
    lines.push(Line::default());
    lines.extend(content.lines().map(|line| Line::raw(line.to_string())));
    Some(Document {
        title: file.path,
        lines,
    })
}

/// `relish source [query]` — open the reader, optionally mid-search.
pub async fn run(query: Option<String>) -> Result<(), RelishError> {
    reader::run("files", documents()?, query).await
}

/// `relish source --extract <dir>` — write the embedded tree into a new or
/// empty directory. CI builds the result to prove the bundle is complete.
pub fn extract(dir: &Path) -> Result<(), RelishError> {
    let written = source_bundle::extract(dir)?;
    println!(
        "wrote {written} files (sha256 {}) to {}",
        source_bundle::digest()?,
        dir.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::reader::{fuzzy_rank, line_text};

    fn titles() -> Vec<String> {
        documents()
            .unwrap()
            .into_iter()
            .map(|document| document.title)
            .collect()
    }

    #[test]
    fn documents_cover_the_source_tree_sorted_by_path() {
        let titles = titles();
        assert!(
            titles.len() > 500,
            "expected the whole bundle, got {}",
            titles.len()
        );
        assert!(titles.contains(&"src/bin/relish.rs".to_string()));
        assert!(titles.contains(&"src/relish/source.rs".to_string()));
        assert!(titles.contains(&"Cargo.toml".to_string()));
        // Sorted for a stable list pane.
        let mut sorted = titles.clone();
        sorted.sort();
        assert_eq!(titles, sorted);
    }

    #[test]
    fn binary_files_are_left_out() {
        let titles = titles();
        assert!(
            !titles.contains(&"tests/fixtures/age/sealed-root-key.age".to_string()),
            "binary fixtures have nothing to read"
        );
    }

    #[test]
    fn fuzzy_ebpf_finds_the_ebpf_sources() {
        let titles = titles();
        let ranked = fuzzy_rank("ebpf", &titles);
        assert!(!ranked.is_empty());
        let top: Vec<&str> = ranked.iter().take(5).map(|&i| titles[i].as_str()).collect();
        assert!(
            top.iter().any(|path| path.contains("ebpf")),
            "expected an ebpf path among the top hits, got {top:?}"
        );
    }

    #[test]
    fn documents_start_with_their_path_and_carry_the_content() {
        let documents = documents().unwrap();
        let this_file = documents
            .iter()
            .find(|doc| doc.title == "src/relish/source.rs")
            .expect("this file is embedded");
        assert_eq!(line_text(&this_file.lines[0]), "src/relish/source.rs");
        assert!(
            this_file
                .lines
                .iter()
                .any(|line| line_text(line).contains("fuzzy-searchable")),
            "file content is present"
        );
    }
}
