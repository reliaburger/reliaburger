//! `relish manual` — the embedded, searchable reference.
//!
//! Chapters live in `docs/manual/*.md` and are compiled into the binary
//! (`assets`), rendered for the terminal (`render`) through the shared
//! reader, or served as one HTML page (`web`). `relish manual examples`
//! drops the embedded example configs into the working directory so every
//! chapter's commands are runnable as printed.

pub mod assets;
pub mod render;
pub mod web;

use std::path::Path;

use super::RelishError;
use super::reader::{self, Document};

/// The chapters as reader documents (rendered lines + titles).
pub fn documents() -> Vec<Document> {
    assets::chapters()
        .into_iter()
        .map(|chapter| Document {
            title: chapter.title,
            lines: render::markdown_to_lines(&chapter.markdown),
        })
        .collect()
}

/// `relish manual` — open the reader.
pub async fn run() -> Result<(), RelishError> {
    reader::run("chapters", documents(), None).await
}

/// `relish manual examples` — extract the embedded examples under `dir`.
pub fn examples(dir: &Path) -> Result<(), RelishError> {
    let written = assets::write_examples(dir)?;
    if written.is_empty() {
        println!(
            "examples already present under {}",
            dir.join("examples").display()
        );
    } else {
        for path in &written {
            println!("wrote {}", path.display());
        }
        println!("try: relish apply examples/phase-1/proc-first-run.toml");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_mirror_the_chapters() {
        let documents = documents();
        assert_eq!(documents.len(), assets::chapters().len());
        assert_eq!(documents[0].title, "Getting started");
        assert!(!documents[0].lines.is_empty());
    }
}
