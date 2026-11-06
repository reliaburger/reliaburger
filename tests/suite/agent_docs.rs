//! The agent-facing files agree with the manual chapter they come from
//! (#1090).
//!
//! `docs/manual/16_working-with-an-ai-agent.md` is the one source.
//! `reliaburger::relish::agent::guide::generate` makes `llms.txt`, the
//! website's `/agents` page (HTML and Markdown), the `reliaburger` skill, the
//! Claude Code plugin and marketplace, and the generated block of
//! `AGENTS.md` from it, and writes the chapter's own command table from the
//! clap definition of `relish agent`. This test fails when any file on disk
//! differs from what generation makes, which is how a chapter edit or a new
//! `relish agent` command that skipped `make agent-docs` gets caught. With
//! `RELIABURGER_UPDATE_AGENT_DOCS` set it writes the files instead.

use reliaburger::relish::agent::guide::{
    self, Inputs, ManualEntry, REGENERATE_COMMAND, SKILL_DESCRIPTION_MAX, SKILL_NAME,
};

use super::website::{read, repository};

fn manual_entries() -> Vec<ManualEntry> {
    let directory = repository().join("docs/manual");
    let mut files: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".md"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|file| {
            let text = read(&format!("docs/manual/{file}"));
            let title = text
                .lines()
                .find_map(|line| line.strip_prefix("# "))
                .unwrap_or(&file)
                .trim()
                .to_string();
            ManualEntry { file, title }
        })
        .collect()
}

fn generated() -> Vec<guide::Artefact> {
    let chapter = read(guide::paths::CHAPTER);
    let agents_md = read(guide::paths::AGENTS_MD);
    let manual = manual_entries();
    let inputs = Inputs {
        chapter: &chapter,
        agents_md: &agents_md,
        manual: &manual,
        version: env!("CARGO_PKG_VERSION"),
    };
    guide::generate(&inputs, &guide::agent_command()).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn every_agent_facing_file_matches_the_chapter() {
    let artefacts = generated();
    if std::env::var_os("RELIABURGER_UPDATE_AGENT_DOCS").is_some() {
        for artefact in &artefacts {
            let path = repository().join(artefact.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &artefact.contents).unwrap();
        }
        return;
    }
    let stale = guide::stale_paths(&artefacts, |path| {
        std::fs::read_to_string(repository().join(path)).ok()
    });
    assert!(
        stale.is_empty(),
        "out of date with docs/manual/{}: {stale:?}; run `{REGENERATE_COMMAND}`",
        guide::CHAPTER_FILE
    );
}

#[test]
fn llms_txt_links_every_manual_chapter() {
    let llms = read(guide::paths::LLMS_TXT);
    for entry in manual_entries() {
        assert!(
            llms.contains(&format!("/docs/manual/{})", entry.file)),
            "llms.txt doesn't link {}",
            entry.file
        );
    }
}

#[test]
fn the_skill_follows_the_agent_skills_format() {
    let skill = read(guide::paths::SKILL);
    let front = skill
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(front, _)| front)
        .expect("SKILL.md starts with YAML front matter");
    let name = front
        .lines()
        .find_map(|line| line.strip_prefix("name: "))
        .unwrap();
    assert_eq!(name, SKILL_NAME);
    let directory = std::path::Path::new(guide::paths::SKILL)
        .parent()
        .and_then(|parent| parent.file_name())
        .unwrap();
    assert_eq!(directory, SKILL_NAME, "the name matches its directory");
    let description: String = front
        .lines()
        .find_map(|line| line.strip_prefix("description: "))
        .map(|quoted| serde_json::from_str(quoted).unwrap())
        .unwrap();
    assert!(description.starts_with("Use when"));
    assert!(description.chars().count() <= SKILL_DESCRIPTION_MAX);
    assert!(
        skill.lines().count() < 500,
        "the format advises under 500 lines"
    );
}

#[test]
fn the_marketplace_points_at_the_plugin_and_the_plugin_at_this_version() {
    let marketplace: serde_json::Value =
        serde_json::from_str(&read(guide::paths::MARKETPLACE)).unwrap();
    let plugin: serde_json::Value = serde_json::from_str(&read(guide::paths::PLUGIN)).unwrap();
    let entry = &marketplace["plugins"][0];
    assert_eq!(entry["name"], plugin["name"]);
    let source = entry["source"].as_str().unwrap();
    assert!(source.starts_with("./"));
    let plugin_root = repository().join(source);
    assert!(plugin_root.join(".claude-plugin/plugin.json").is_file());
    assert!(
        plugin_root
            .join("skills")
            .join(SKILL_NAME)
            .join("SKILL.md")
            .is_file()
    );
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));
}
