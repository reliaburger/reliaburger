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

/// The recording's script (`scripts/demo/tour.sh`) reads its commands from
/// the page, and knows how to run and wait for each one. A command added to
/// the tour must be taught to the script too, or the recording drifts.
#[test]
fn the_recording_script_knows_every_tour_command() {
    let output = Command::new("bash")
        .arg(repository().join("scripts/demo/tour.sh"))
        .arg("--check")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
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

/// The `id` of every `<section>` on the homepage, in page order.
fn section_ids(page: &str) -> Vec<Option<String>> {
    page.match_indices("<section")
        .map(|(start, _)| {
            let tag = &page[start..start + page[start..].find('>').unwrap()];
            tag.split(" id=\"")
                .nth(1)
                .map(|rest| rest[..rest.find('"').unwrap()].to_string())
        })
        .collect()
}

/// Every section has a stable `id` and its heading a `#` link to it, so a
/// section can be shared as `reliaburger.com/#install` (#280).
#[test]
fn every_section_can_be_linked_to() {
    let page = read("docs/website/index.html");
    let ids = section_ids(&page);
    assert!(
        ids.iter().all(Option::is_some),
        "a <section> has no id: {ids:?}"
    );
    let ids: Vec<String> = ids.into_iter().flatten().collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "duplicate section ids: {ids:?}");
    for id in [
        "install",
        "tour",
        "start",
        "docs",
        "internals",
        "contributing",
    ] {
        assert!(ids.iter().any(|i| i == id), "no section with id {id:?}");
    }
    // The intro's heading is the page title; every other section's heading
    // carries a permalink.
    for id in ids.iter().filter(|id| *id != "intro") {
        assert!(
            page.contains(&format!("<a class=\"permalink\" href=\"#{id}\"")),
            "section {id:?} has no heading permalink"
        );
    }
    let css = read("docs/website/style.css");
    assert!(
        css.contains("scroll-margin-top"),
        "linked headings need scroll-margin-top"
    );
}

/// A release is published, so the page leads with the one-line install and keeps
/// building from source as the second path (#280).
#[test]
fn the_page_leads_with_the_one_line_install() {
    let page = read("docs/website/index.html");
    let first_snippet = &page[page.find("<pre").unwrap()..];
    let first_snippet = &first_snippet[..first_snippet.find("</pre>").unwrap()];
    assert_eq!(decode(&strip_tags(first_snippet)), INSTALL_LINE);
    let ids: Vec<String> = section_ids(&page).into_iter().flatten().collect();
    let position = |id: &str| ids.iter().position(|i| i == id).unwrap();
    assert!(position("install") < position("tour"));
    assert!(position("install") < position("start"));
    for stale in [
        "Arrives with 0.1.0",
        "Working towards 0.1.0",
        "release-not-published",
    ] {
        assert!(!page.contains(stale), "the page still says {stale:?}");
    }
}

/// The version the bootstrap installs by default is the one the site, the
/// READMEs and the tour chapter announce, so promoting a release can't flip
/// one of them and forget the rest (#317).
#[test]
fn the_site_and_docs_announce_the_version_the_installer_installs() {
    let installer = read("docs/website/install.sh");
    let default = installer
        .split("${RELIABURGER_VERSION:-")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("install.sh has no RELIABURGER_VERSION default");
    let version = default.trim_start_matches('v');
    let notes = format!("https://github.com/reliaburger/reliaburger/releases/tag/{default}");

    let page = read("docs/website/index.html");
    for expected in [
        format!("{version} PROTOTYPE"),
        format!("Install {version} "),
        format!("<strong>{version} (Prototype)</strong>"),
        format!("href=\"{notes}\""),
    ] {
        assert!(page.contains(&expected), "index.html lacks {expected:?}");
    }
    assert_eq!(
        page.matches("/releases/tag/").count(),
        page.matches(&notes).count(),
        "index.html links release notes other than {notes}"
    );
    for (path, expected) in [
        ("README.md", format!("{version} was released on")),
        ("docs/README.md", notes.clone()),
        ("docs/quickstart.md", format!("signed {version} release")),
        (
            "docs/manual/08_five-minute-tour.md",
            format!("signed {version} release"),
        ),
        ("docs/linux-servers.md", format!("VERSION=\"{default}\"")),
        ("docs/releasing.md", format!("(`{default}` today)")),
    ] {
        assert!(read(path).contains(&expected), "{path} lacks {expected:?}");
    }
}

/// Every command snippet gets a copy button from the page's script, so
/// without JavaScript there's no dead button, just selectable text (#280).
#[test]
fn command_snippets_are_copyable_and_still_selectable_without_script() {
    let page = read("docs/website/index.html");
    assert!(page.contains("<script src=\"./assets/copy.js\" defer></script>"));
    assert!(
        !page.contains("class=\"copy\""),
        "copy buttons come from the script, not the markup"
    );
    let script = read("docs/website/assets/copy.js");
    assert!(script.contains("querySelectorAll(\"pre\")"));
    assert!(script.contains("aria-live"));
    assert!(script.contains("\"Copied\""));
    let css = read("docs/website/style.css");
    assert!(!css.contains("user-select: none"));
}

/// Every release carries the PDFs, so the page links the stable "latest"
/// release assets rather than sending people to CI artefacts (#280).
#[test]
fn the_pdfs_come_from_the_latest_release() {
    let page = read("docs/website/index.html");
    for pdf in [
        "reliaburger-whitepaper.pdf",
        "building-reliaburger.pdf",
        "reliaburger-design-docs.pdf",
        "reliaburger-roadmap.pdf",
    ] {
        let url =
            format!("https://github.com/reliaburger/reliaburger/releases/latest/download/{pdf}");
        assert!(page.contains(&url), "no link to {url}");
    }
    assert!(
        !page.contains("<strong>Artifacts</strong>"),
        "the page still sends people to CI artefacts for the PDFs"
    );
}
