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

/// A chapter name `relish manual CHAPTER` couldn't resolve.
#[derive(Debug, thiserror::Error)]
pub enum ManualError {
    /// Nothing matched, or more than one chapter did.
    #[error("no single manual chapter matches {query:?}; chapters: {}", .available.join(", "))]
    UnknownChapter {
        query: String,
        available: Vec<String>,
    },
}

/// A chapter's short name: its file name without the number prefix and
/// extension, e.g. `five-minute-tour` for `08_five-minute-tour.md`.
pub fn slug(file: &str) -> &str {
    let stem = file.strip_suffix(".md").unwrap_or(file);
    stem.split_once('_').map_or(stem, |(_, rest)| rest)
}

/// Resolve what a reader typed to a chapter index: an exact short name
/// (`chaos`), a part of exactly one short name (`tour`), or a part of
/// exactly one title. Case doesn't matter.
pub fn find_chapter(chapters: &[assets::Chapter], query: &str) -> Result<usize, ManualError> {
    let wanted = query.trim().to_lowercase();
    let unique = |matches: Vec<usize>| (matches.len() == 1).then(|| matches[0]);
    let by_slug = |exact: bool| {
        chapters
            .iter()
            .enumerate()
            .filter(|(_, chapter)| {
                let slug = slug(&chapter.file);
                if exact {
                    slug == wanted
                } else {
                    slug.contains(&wanted)
                }
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };
    let by_title = chapters
        .iter()
        .enumerate()
        .filter(|(_, chapter)| chapter.title.to_lowercase().contains(&wanted))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let found = if wanted.is_empty() {
        None
    } else {
        unique(by_slug(true))
            .or_else(|| unique(by_slug(false)))
            .or_else(|| unique(by_title))
    };
    found.ok_or_else(|| ManualError::UnknownChapter {
        query: query.to_string(),
        available: chapters
            .iter()
            .map(|chapter| slug(&chapter.file).to_string())
            .collect(),
    })
}

/// `relish manual [CHAPTER]` — open the reader, on `chapter` if given.
pub async fn run(chapter: Option<&str>) -> Result<(), RelishError> {
    let Some(query) = chapter else {
        return reader::run("chapters", documents(), None).await;
    };
    let index = find_chapter(&assets::chapters(), query)?;
    let mut state = reader::ReaderState::new(documents());
    state.list_title = "chapters".to_string();
    state.open_document(index);
    reader::run_state(state).await
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
    fn tour_finds_the_five_minute_tour() {
        let chapters = assets::chapters();
        let index = find_chapter(&chapters, "tour").unwrap();
        assert_eq!(chapters[index].file, "08_five-minute-tour.md");
        assert_eq!(chapters[index].title, "Five-minute tour");
    }

    #[test]
    fn chapters_resolve_by_short_name_part_or_title() {
        let chapters = assets::chapters();
        let file = |query: &str| {
            chapters[find_chapter(&chapters, query).unwrap()]
                .file
                .clone()
        };
        assert_eq!(file("getting-started"), "00_getting-started.md");
        assert_eq!(file("CHAOS"), "05_chaos.md");
        assert_eq!(file("Breaking things"), "05_chaos.md");
    }

    #[test]
    fn unknown_or_ambiguous_chapters_list_the_choices() {
        let chapters = assets::chapters();
        for query in ["no-such-chapter", "", "e"] {
            let error = find_chapter(&chapters, query).unwrap_err().to_string();
            assert!(error.contains("five-minute-tour"), "{query}: {error}");
        }
    }

    #[test]
    fn slugs_drop_the_number_and_extension() {
        assert_eq!(slug("08_five-minute-tour.md"), "five-minute-tour");
        assert_eq!(slug("notes.md"), "notes");
    }

    #[test]
    fn documents_mirror_the_chapters() {
        let documents = documents();
        assert_eq!(documents.len(), assets::chapters().len());
        assert_eq!(documents[0].title, "Getting started");
        assert!(!documents[0].lines.is_empty());
    }
}
