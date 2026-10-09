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

const BURGER_URL: &str = "https://reliaburger.com/demo/burger.tar.gz";
const BURGER_SOURCE: &str = "examples/demo/burger";
const BURGER_PACK: &str = "tar -czf docs/website/demo/burger.tar.gz -C examples/demo burger";
/// What the served tarball carries: the demo app's source and nothing else.
/// `burger` itself is `go build`'s output, ignored by version control, so a
/// local build doesn't fail this.
const BURGER_FILES: &[&str] = &[
    "Dockerfile",
    "batch.go",
    "batch_test.go",
    "burger.toml",
    "go.mod",
    "jobs.toml",
    "main.go",
    "main_test.go",
];

fn burger_files_on_disk() -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(repository().join(BURGER_SOURCE))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "burger")
        .collect();
    files.sort();
    files
}

/// The tour's build step downloads `demo/burger.tar.gz`. Like the podinfo
/// manifest, it isn't committed: the Pages workflow packs it from
/// `examples/demo/burger` at deploy time, so the site serves the directory CI
/// dry-runs (`tests/examples.rs`) and `relish manual examples` ships.
#[test]
fn the_demo_app_is_published_from_the_tested_example() {
    assert!(read("docs/website/index.html").contains(BURGER_URL));
    assert!(read("docs/manual/08_five-minute-tour.md").contains(BURGER_URL));
    let workflow = read(".github/workflows/static.yml");
    assert!(
        workflow.contains(BURGER_PACK),
        "the Pages workflow must pack {BURGER_SOURCE} as demo/burger.tar.gz"
    );
    assert_eq!(burger_files_on_disk(), BURGER_FILES);

    let config = reliaburger::config::Config::from_file(
        &repository().join(BURGER_SOURCE).join("burger.toml"),
    )
    .unwrap();
    let build = &config.build["burger"];
    assert_eq!(build.context, Path::new("."));
    assert_eq!(build.destination, "pickle://burger:v1");
    // The app runs what the build pushed, by the bare name nodes resolve
    // through Pickle.
    assert_eq!(config.app["burger"].image.as_deref(), Some("burger:v1"));
}

/// The workflow's `tar` line, run for real: unpacked, the tarball is a
/// `burger/` directory holding the example's files byte for byte, which is
/// what `curl … | tar xz` followed by `relish build burger/burger.toml` needs.
#[test]
fn the_packed_demo_app_unpacks_to_the_example_directory() {
    let scratch = tempfile::tempdir().unwrap();
    let tarball = scratch.path().join("burger.tar.gz");
    let arguments: Vec<String> = BURGER_PACK
        .split_whitespace()
        .skip(1)
        .map(|argument| {
            if argument == "docs/website/demo/burger.tar.gz" {
                tarball.display().to_string()
            } else {
                argument.to_string()
            }
        })
        .collect();
    let status = Command::new("tar")
        .args(&arguments)
        .current_dir(repository())
        // macOS's bsdtar would otherwise add `._*` resource-fork entries.
        .env("COPYFILE_DISABLE", "1")
        .status()
        .unwrap();
    assert!(status.success());

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
        std::fs::File::open(&tarball).unwrap(),
    ));
    let mut unpacked = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        assert!(
            path.starts_with("burger"),
            "{} is outside burger/",
            path.display()
        );
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        let source = std::fs::read(repository().join(BURGER_SOURCE).join(&name)).unwrap();
        assert_eq!(bytes, source, "{name} differs from the example");
        unpacked.push(name);
    }
    unpacked.retain(|name| name != "burger");
    unpacked.sort();
    assert_eq!(unpacked, BURGER_FILES);
}

/// The README's quick start is a shortened tour: every `relish` line in it
/// must be one the tour runs (and so one the tests above parse), and it shows
/// the build step too.
#[test]
fn readme_quick_start_is_part_of_the_tour() {
    let readme = read("README.md");
    let start = readme
        .find(INSTALL_LINE)
        .expect("README lost the install line");
    let end = start + readme[start..].find("```").unwrap();
    let lines: Vec<String> = readme[start..end]
        .lines()
        .map(|line| line.split(" #").next().unwrap().trim().to_string())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let tour = relish_lines(&homepage_commands());
    for line in relish_lines(&lines) {
        assert!(
            tour.contains(&line),
            "README runs {line:?}, which the tour doesn't"
        );
    }
    assert!(lines.iter().any(|line| line.contains(BURGER_URL)));
    assert!(lines.contains(&"relish build burger/burger.toml".to_string()));
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

/// The byte ranges of every `<details>…</details>` element on the page. The
/// page doesn't nest them, which the assertion below keeps true.
fn details_ranges(page: &str) -> Vec<std::ops::Range<usize>> {
    let opens: Vec<usize> = page.match_indices("<details").map(|(at, _)| at).collect();
    let closes: Vec<usize> = page
        .match_indices("</details>")
        .map(|(at, tag)| at + tag.len())
        .collect();
    assert_eq!(opens.len(), closes.len(), "unbalanced <details>");
    let ranges: Vec<_> = opens.into_iter().zip(closes).map(|(o, c)| o..c).collect();
    for range in &ranges {
        assert!(range.start < range.end, "<details> closes before it opens");
    }
    for pair in ranges.windows(2) {
        assert!(pair[0].end <= pair[1].start, "nested <details>");
    }
    ranges
}

/// People didn't notice the tour while the whole section sat in one collapsed
/// `<details>`. The heading and the recording stay in plain view, loaded
/// without a click; only the command list folds away.
#[test]
fn the_tour_recording_is_always_visible_and_only_the_commands_fold() {
    let page = read("docs/website/index.html");
    let folded = details_ranges(&page);
    let inside = |at: usize| folded.iter().any(|range| range.contains(&at));

    for marker in [
        "<section id=\"tour\"",
        "id=\"tour-title\"",
        "id=\"tour-recording\"",
    ] {
        let at = page.find(marker).unwrap_or_else(|| panic!("no {marker}"));
        assert!(!inside(at), "{marker} is inside a <details>");
    }

    let commands = page
        .find("<details class=\"tour-commands\" id=\"tour-commands\">")
        .expect("the tour's commands aren't in their own <details>");
    let rest = &page[commands..];
    let summary = &rest[rest.find("<summary>").unwrap()..rest.find("</summary>").unwrap()];
    assert!(
        strip_tags(summary).starts_with("Show the commands"),
        "the commands' summary reads {summary:?}"
    );
    let fold = folded.iter().find(|range| range.start == commands).unwrap();
    assert!(fold.start > page.find("id=\"tour-recording\"").unwrap());
    let steps = page.find("<ol class=\"tour-steps\">").unwrap();
    assert!(fold.contains(&steps), "the tour's steps aren't folded");
    // Every tour command is in the fold, bar the pointer to the CLI's own copy.
    for (at, _) in page.match_indices("<code data-tour>") {
        let command = &page[at..at + page[at..].find("</code>").unwrap()];
        if strip_tags(command) == "relish manual tour" {
            continue;
        }
        assert!(fold.contains(&at), "{command:?} is outside the fold");
    }

    // The recording loads by itself, not when something is opened.
    let player = read("docs/website/assets/tour-player.js");
    assert!(player.contains("IntersectionObserver"));
    assert!(
        !player.contains("\"toggle\""),
        "the player still waits for a toggle"
    );
    assert!(!player.contains("closest(\"details\")"));
}

/// Each tour step is a three-column grid: the step number, one command block,
/// one paragraph (`.tour-steps li` in `style.css`). A second paragraph wraps
/// onto a new row and lands in the 2.5rem number column, one word per line,
/// as step 06 did after its batch-jobs note was added (8 October 2026).
#[test]
fn every_tour_step_is_one_command_block_then_one_paragraph() {
    let page = read("docs/website/index.html");
    let start = page.find("<ol class=\"tour-steps\">").unwrap();
    let list = &page[start..start + page[start..].find("</ol>").unwrap()];
    for (number, step) in list.split("<li>").skip(1).enumerate() {
        let step = &step[..step.find("</li>").expect("unterminated tour step")];
        let label = format!("tour step {:02}", number + 1);
        assert_eq!(
            step.matches("<p>").count(),
            1,
            "{label} needs exactly one paragraph"
        );
        assert!(
            step.matches("<pre>").count() <= 1,
            "{label} has more than one command block"
        );
        if let Some(pre) = step.find("<pre>") {
            assert!(
                pre < step.find("<p>").unwrap(),
                "{label}'s paragraph comes before its commands"
            );
        }
    }
}
