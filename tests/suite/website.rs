//! Keep the five-minute tour honest (Z5.4).
//!
//! The homepage (`docs/website/index.html`, commands marked `data-tour`) and
//! the manual chapter (`docs/manual/08_five-minute-tour.md`, lines in its
//! ```sh blocks) show the same tour. Every `relish …` line in them must parse
//! with the real CLI definition: we run the compiled binary with
//! `RELISH_PARSE_ONLY=1`, which parses the arguments and exits without doing
//! anything. The two copies must list the same commands, and the demo
//! manifest the tour applies must exist and be published by the Pages workflow.
//!
//! `scripts/ci/select-jobs.sh` counts `docs/website/index.html` as code so an
//! edit to the page alone still runs this test.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Tour commands that describe CLI behaviour still being built on other
/// branches. Each must FAIL to parse today; once it parses, the test fails
/// and asks for its entry to be removed, so an exemption can't outlive its
/// reason.
const PENDING: &[(&str, &str)] = &[];

const DEMO_MANIFEST_PENDING: bool = false;

const INSTALL_LINE: &str = "curl -fsSL https://reliaburger.com/install.sh | sh";
const DEMO_URL: &str = "https://reliaburger.com/demo/podinfo.yaml";
const DEMO_SOURCE: &str = "examples/kubernetes/podinfo.yaml";

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repository().join(path)).unwrap()
}

/// The text of every `<code data-tour>` element, tags stripped and entities
/// decoded, so `<var>NODE</var>` reads as `NODE`.
fn homepage_commands() -> Vec<String> {
    let page = read("docs/website/index.html");
    let mut commands = Vec::new();
    let mut rest = page.as_str();
    while let Some(start) = rest.find("<code data-tour>") {
        rest = &rest[start + "<code data-tour>".len()..];
        let end = rest.find("</code>").expect("unterminated tour command");
        commands.push(decode(&strip_tags(&rest[..end])));
        rest = &rest[end..];
    }
    commands
}

fn strip_tags(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text
}

fn decode(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Every non-comment line inside the chapter's ```sh blocks.
fn manual_commands() -> Vec<String> {
    let chapter = read("docs/manual/08_five-minute-tour.md");
    let mut commands = Vec::new();
    let mut in_block = false;
    for line in chapter.lines() {
        match line.trim() {
            "```sh" => in_block = true,
            "```" => in_block = false,
            text if in_block && !text.is_empty() && !text.starts_with('#') => {
                commands.push(text.to_string())
            }
            _ => {}
        }
    }
    commands
}

fn relish_lines(commands: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = commands
        .iter()
        .filter(|command| command.starts_with("relish "))
        .cloned()
        .collect();
    lines.sort();
    lines.dedup();
    lines
}

fn parses(command: &str) -> Result<(), String> {
    let arguments: Vec<&str> = command.split_whitespace().skip(1).collect();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(&arguments)
        .env("RELISH_PARSE_ONLY", "1")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env_remove("RELIABURGER_ENDPOINT")
        .output()
        .unwrap();
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

#[test]
fn every_tour_command_parses_with_the_real_cli() {
    let mut commands = relish_lines(&homepage_commands());
    commands.extend(relish_lines(&manual_commands()));
    commands.sort();
    commands.dedup();
    assert!(
        commands.len() >= 10,
        "found only {commands:?}; did the markup change?"
    );

    let mut failures = Vec::new();
    for command in &commands {
        let pending = PENDING.iter().find(|(line, _)| line == command);
        match (parses(command), pending) {
            (Ok(()), None) | (Err(_), Some(_)) => {}
            (Err(error), None) => failures.push(format!("{command}\n{error}")),
            (Ok(()), Some((_, item))) => failures.push(format!(
                "{command} parses now; remove its {item} entry from PENDING in tests/suite/website.rs"
            )),
        }
    }
    for (line, item) in PENDING {
        if !commands.iter().any(|command| command == line) {
            failures.push(format!(
                "PENDING lists {line:?} ({item}) but the tour no longer uses it"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn homepage_and_manual_show_the_same_tour() {
    let homepage = homepage_commands();
    let manual = manual_commands();
    // The page also points at the chapter itself; the chapter doesn't need to.
    let mut steps = relish_lines(&homepage);
    steps.retain(|command| command != "relish manual tour");
    assert_eq!(steps, relish_lines(&manual));
    assert_eq!(homepage.first().map(String::as_str), Some(INSTALL_LINE));
    assert_eq!(manual.first().map(String::as_str), Some(INSTALL_LINE));
    assert!(
        repository().join("docs/website/install.sh").is_file(),
        "the install line fetches /install.sh from the site"
    );
}

#[test]
fn manual_tour_opens_the_tour_chapter() {
    use reliaburger::relish::manual::{assets, find_chapter};
    let chapters = assets::chapters();
    let index = find_chapter(&chapters, "tour").unwrap();
    assert_eq!(chapters[index].file, "08_five-minute-tour.md");
}

#[test]
fn the_demo_manifest_is_published_from_the_tested_example() {
    let page = read("docs/website/index.html");
    assert!(page.contains(DEMO_URL));
    let workflow = read(".github/workflows/static.yml");
    assert!(
        workflow.contains(&format!("cp {DEMO_SOURCE} docs/website/demo/podinfo.yaml")),
        "the Pages workflow must copy {DEMO_SOURCE} to demo/podinfo.yaml"
    );
    let exists = Path::new(&repository()).join(DEMO_SOURCE).is_file();
    if DEMO_MANIFEST_PENDING {
        assert!(
            !exists,
            "{DEMO_SOURCE} exists now; set DEMO_MANIFEST_PENDING to false (Z1.5)"
        );
    } else {
        assert!(exists, "{DEMO_SOURCE} is missing");
    }
}
