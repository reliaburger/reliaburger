//! Embedded manual chapters and example configs.
//!
//! Both are compiled into the binary with `rust-embed`, the same way the
//! Brioche dashboard ships its static assets: the manual works on a machine
//! that has nothing but the `relish` binary on it.

use std::path::{Path, PathBuf};

use rust_embed::Embed;

/// `docs/manual/*.md`, compiled in at build time.
#[derive(Embed)]
#[folder = "docs/manual/"]
pub struct ManualAssets;

/// `examples/**`, compiled in at build time.
#[derive(Embed)]
#[folder = "examples/"]
pub struct ExampleAssets;

/// One manual chapter.
#[derive(Debug, Clone)]
pub struct Chapter {
    /// Embedded file name, e.g. `00_getting-started.md`.
    pub file: String,
    /// The chapter's first `# ` heading (falls back to the file name).
    pub title: String,
    pub markdown: String,
}

/// Every embedded chapter, in file-name (chapter-number) order.
pub fn chapters() -> Vec<Chapter> {
    let mut files: Vec<String> = ManualAssets::iter()
        .map(|file| file.to_string())
        .filter(|file| file.ends_with(".md"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let asset = ManualAssets::get(&file)?;
            let markdown = String::from_utf8_lossy(asset.data.as_ref()).into_owned();
            let title = markdown
                .lines()
                .find_map(|line| line.strip_prefix("# "))
                .unwrap_or(&file)
                .trim()
                .to_string();
            Some(Chapter {
                file,
                title,
                markdown,
            })
        })
        .collect()
}

/// Write the embedded examples under `dir/examples/…`, skipping files that
/// already exist (re-running never clobbers local edits). Returns the paths
/// actually written.
pub fn write_examples(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for file in ExampleAssets::iter() {
        let target = dir.join("examples").join(file.as_ref());
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Iterated names always resolve; the embed is read-only at runtime.
        let asset = ExampleAssets::get(&file).expect("iterated asset exists");
        std::fs::write(&target, asset.data.as_ref())?;
        written.push(target);
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapters_are_present_ordered_and_titled() {
        let chapters = chapters();
        assert!(chapters.len() >= 6);
        // File-name order is chapter order.
        let files: Vec<_> = chapters.iter().map(|c| c.file.clone()).collect();
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
        // The first chapter is getting started, with its heading as title.
        assert_eq!(chapters[0].file, "00_getting-started.md");
        assert_eq!(chapters[0].title, "Getting started");
        assert!(chapters[0].markdown.contains("relish setup"));
    }

    #[test]
    fn every_chapter_has_a_heading_derived_title() {
        for chapter in chapters() {
            assert!(
                !chapter.title.is_empty() && !chapter.title.starts_with('#'),
                "bad title for {}",
                chapter.file
            );
        }
    }

    #[test]
    fn write_examples_extracts_the_embedded_tree() {
        let dir = tempfile::tempdir().unwrap();
        let written = write_examples(dir.path()).unwrap();
        assert!(!written.is_empty());

        let sample = dir.path().join("examples/phase-1/proc-first-run.toml");
        assert!(sample.is_file());
        let embedded = ExampleAssets::get("phase-1/proc-first-run.toml").unwrap();
        assert_eq!(std::fs::read(&sample).unwrap(), embedded.data.as_ref());
    }

    #[test]
    fn write_examples_skips_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("examples/phase-1/proc-first-run.toml");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "local edits").unwrap();

        let written = write_examples(dir.path()).unwrap();

        assert!(!written.contains(&target));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "local edits");
    }
}
