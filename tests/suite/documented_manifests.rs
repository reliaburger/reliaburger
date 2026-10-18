//! Every manifest we publish for applying as written passes the validation
//! `relish apply` and the API apply. The manual's blocks are excerpts (a
//! health section alone, a job naming an app defined earlier), so they're
//! not checked here. A manifest that only parses can still be refused by the
//! cluster, as the quickstart's demo was when a health timeout outlasted its
//! interval.

use std::path::{Path, PathBuf};

use reliaburger::config::Config;

fn files_with_extension(directory: &Path, extension: &str, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_with_extension(&path, extension, found);
        } else if path.extension().is_some_and(|found| found == extension) {
            found.push(path);
        }
    }
}

/// The ```toml blocks of a Markdown file.
fn toml_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match current.as_mut() {
            None if line.trim_start().starts_with("```toml") => current = Some(String::new()),
            Some(_) if line.trim_start().starts_with("```") => {
                blocks.extend(current.take());
            }
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
            None => {}
        }
    }
    blocks
}

/// A manifest that parses and declares an app or a job: fragments and node
/// configs don't, and are checked elsewhere.
fn workload_manifest(toml: &str) -> Option<Config> {
    let config = Config::parse(toml).ok()?;
    (!config.app.is_empty() || !config.job.is_empty()).then_some(config)
}

#[test]
fn every_published_manifest_passes_apply_validation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources: Vec<(String, String)> = Vec::new();

    let mut markdown = Vec::new();
    files_with_extension(&root.join("docs/website"), "md", &mut markdown);
    for file in ["README.md", "docs/quickstart.md", "docs/linux-servers.md"] {
        markdown.push(root.join(file));
    }
    for path in markdown {
        let text = std::fs::read_to_string(&path).unwrap();
        for (index, block) in toml_blocks(&text).into_iter().enumerate() {
            let name = format!("{} block {}", path.display(), index + 1);
            sources.push((name, block));
        }
    }

    let mut examples = Vec::new();
    files_with_extension(&root.join("examples"), "toml", &mut examples);
    for path in examples {
        sources.push((
            path.display().to_string(),
            std::fs::read_to_string(&path).unwrap(),
        ));
    }

    let mut checked = 0;
    let mut refused = Vec::new();
    for (name, toml) in &sources {
        let Some(config) = workload_manifest(toml) else {
            continue;
        };
        checked += 1;
        if let Err(error) = config.validate_intrinsic() {
            refused.push(format!("{name}: {error}"));
        }
    }
    assert!(
        checked >= 20,
        "found only {checked} published manifests; did the search paths move?"
    );
    assert!(
        refused.is_empty(),
        "published manifests the cluster would refuse:\n{}",
        refused.join("\n")
    );
}
