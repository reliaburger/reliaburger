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
        format!("<strong>{version}: "),
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

// --- What the installed release has (#688) ---------------------------------

const RELEASED_CLI: &str = "scripts/release/released-cli.tsv";

/// The tag `install.sh` fetches by default, such as `v0.2.0`.
fn installer_default() -> String {
    read("docs/website/install.sh")
        .split("${RELIABURGER_VERSION:-")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("install.sh has no RELIABURGER_VERSION default")
        .to_string()
}

fn released_cli() -> reliaburger::relish::released_cli::ReleasedCli {
    reliaburger::relish::released_cli::ReleasedCli::parse(&read(RELEASED_CLI)).unwrap()
}

/// What docs write next to a command the installed release lacks, e.g.
/// `0.2.0, not in 0.1.6`: this build's version against the installer's.
fn unreleased_label() -> String {
    let upcoming = env!("CARGO_PKG_VERSION").split('-').next().unwrap();
    released_cli().unreleased_label(upcoming)
}

/// The README with its generated command list cut out, and that list.
fn readme_split() -> (String, String) {
    use reliaburger::relish::command_reference::{END_MARKER, START_MARKER};
    let readme = read("README.md");
    let start = readme.find(START_MARKER).unwrap();
    let end = readme.find(END_MARKER).unwrap();
    (
        format!("{}{}", &readme[..start], &readme[end..]),
        readme[start..end].to_string(),
    )
}

/// The `relish …` commands in one Markdown block: every line of a fenced
/// block, or every inline code span of a paragraph.
fn block_commands(block: &str) -> Vec<String> {
    if block.trim_start().starts_with("```") {
        return block
            .lines()
            .map(|line| line.split(" #").next().unwrap().trim().to_string())
            .filter(|line| line.starts_with("relish "))
            .collect();
    }
    block
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("relish "))
        .map(str::to_string)
        .collect()
}

/// Every `relish …` mention in a Markdown document, each with its paragraph
/// (or its fenced block) as the context a version label has to be in.
fn markdown_mentions(document: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut block = String::new();
    let mut in_fence = false;
    for line in document.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence && !in_fence {
            blocks.push(std::mem::take(&mut block));
        }
        if !in_fence && !fence && line.trim().is_empty() {
            blocks.push(std::mem::take(&mut block));
            continue;
        }
        block.push_str(line);
        block.push('\n');
        if fence && in_fence {
            blocks.push(std::mem::take(&mut block));
        }
        if fence {
            in_fence = !in_fence;
        }
    }
    blocks.push(block);
    blocks
        .iter()
        .flat_map(|block| {
            block_commands(block)
                .into_iter()
                .map(|command| (command, block.clone()))
        })
        .collect()
}

/// Every `relish …` line inside a `<code>` element on the homepage that isn't
/// a tour step, with the innermost `<details>` (or else `<section>`) around it.
fn homepage_mentions() -> Vec<(String, String)> {
    let page = read("docs/website/index.html");
    let enclosing = |at: usize, open: &str, close: &str| -> Option<String> {
        let start = page[..at].rfind(open)?;
        if page[start..at].contains(close) {
            return None;
        }
        let end = at + page[at..].find(close)? + close.len();
        Some(page[start..end].to_string())
    };
    let mut mentions = Vec::new();
    for (at, _) in page.match_indices("<code") {
        let tag_end = at + page[at..].find('>').unwrap();
        if page[at..tag_end].contains("data-tour") {
            continue;
        }
        let end = tag_end + page[tag_end..].find("</code>").unwrap();
        let text = decode(&strip_tags(&page[tag_end + 1..end]));
        for line in text.lines().map(str::trim) {
            if !line.starts_with("relish ") {
                continue;
            }
            let context = enclosing(at, "<details", "</details>")
                .or_else(|| enclosing(at, "<section", "</section>"))
                .unwrap_or_default();
            mentions.push((line.to_string(), decode(&strip_tags(&context))));
        }
    }
    mentions
}

/// A visitor installs the release `install.sh` names, so every command the
/// site, the README and the tour offer must exist in it, or say which
/// release adds it. The tour itself may only use released commands.
#[test]
fn every_advertised_command_exists_in_the_installed_release() {
    let released = released_cli();
    let installer = installer_default();
    assert_eq!(
        released.version(),
        installer,
        "{RELEASED_CLI} describes {} but install.sh installs {installer}; regenerate it with \
         `python3 scripts/release/released_cli.py {installer} > {RELEASED_CLI}`",
        released.version()
    );
    let label = unreleased_label();
    let mut failures = Vec::new();

    let (readme, generated) = readme_split();
    let start = readme.find(INSTALL_LINE).unwrap();
    let end = start + readme[start..].find("```").unwrap();
    let quick_start: Vec<String> = readme[start..end]
        .lines()
        .map(|line| line.split(" #").next().unwrap().trim().to_string())
        .collect();
    let mut tour = relish_lines(&homepage_commands());
    tour.extend(relish_lines(&manual_commands()));
    tour.extend(relish_lines(&quick_start));
    for command in &tour {
        if let Err(rejection) = released.check(command) {
            failures.push(format!("the tour runs `{command}`: {rejection}"));
        }
    }

    let mut entries = 0;
    for line in generated.lines() {
        let Some(synopsis) = line.trim_start().strip_prefix("- `") else {
            continue;
        };
        let synopsis = &synopsis[..synopsis.find('`').unwrap()];
        entries += 1;
        let labelled = line.contains(&format!("*({label})*"));
        match (released.check(synopsis), labelled) {
            (Ok(()), false) | (Err(_), true) => {}
            (Ok(()), true) => failures.push(format!(
                "README labels `{synopsis}` ({label}), but {installer} has it"
            )),
            (Err(rejection), false) => failures.push(format!(
                "README lists `{synopsis}` unlabelled: {rejection}; run `make readme-commands`"
            )),
        }
    }
    assert!(entries > 50, "found only {entries} README command entries");

    let mut mentions = markdown_mentions(&readme);
    mentions.extend(markdown_mentions(&read(
        "docs/manual/08_five-minute-tour.md",
    )));
    mentions.extend(homepage_mentions());
    assert!(mentions.len() > 20, "found only {mentions:?}");

    // Once a release is installed, labels naming the one before it are stale.
    let installed = installer.trim_start_matches('v');
    let stale = regex::Regex::new(r"(\d+\.\d+\.\d+), not in (\d+\.\d+\.\d+)").unwrap();
    for path in [
        "README.md",
        "docs/README.md",
        "docs/website/index.html",
        "docs/manual/08_five-minute-tour.md",
        "docs/manual/14_batch-jobs.md",
    ] {
        for found in stale.captures_iter(&read(path)) {
            if &found[2] != installed || &found[1] == installed {
                failures.push(format!(
                    "{path} says \"{}\", but install.sh installs {installed}",
                    &found[0]
                ));
            }
        }
    }
    for (command, context) in &mentions {
        if let Err(rejection) = released.check(command)
            && !context.contains(&label)
        {
            failures.push(format!(
                "`{command}` is offered without \"{label}\" beside it: {rejection}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The version in a cast's header title, `Reliaburger 0.1.4: …`.
fn recorded_version(cast: &str) -> String {
    let header: serde_json::Value =
        serde_json::from_str(read(cast).lines().next().unwrap()).unwrap();
    let title = header["title"].as_str().expect("the cast has no title");
    title
        .strip_prefix("Reliaburger ")
        .and_then(|rest| rest.split(':').next())
        .filter(|version| version.split('.').count() == 3)
        .unwrap_or_else(|| panic!("{cast}'s title {title:?} names no version"))
        .to_string()
}

/// `scripts/demo/tour.sh --record … --install vX.Y.Z` writes the version
/// into the cast's title, the page's caption names it, and it's the one the
/// installer fetches, so a promotion can't forget the re-record (#688).
#[test]
fn the_tour_recording_matches_the_installer_version() {
    let recorded = recorded_version("docs/website/assets/tour.cast");
    let page = read("docs/website/index.html");
    assert!(
        page.contains(&format!("against the published {recorded} install")),
        "the tour's caption doesn't say it was recorded against {recorded}"
    );
    assert!(
        read("scripts/demo/tour.sh").contains("--title \"Reliaburger ${TITLE_VERSION}:"),
        "tour.sh no longer writes the version into the cast's title"
    );
    let installer = installer_default();
    assert_eq!(
        format!("v{recorded}"),
        installer,
        "the tour was recorded against {recorded}, but install.sh installs {installer}; \
         re-record it (docs/website/README.md, \"Re-recording the tour\")"
    );
}

/// Each release in the roadmap's list with the name it gives it
/// (`- [ ] **0.3.0: "Bare metal in an hour"**`), lower-cased.
fn roadmap_release_names() -> Vec<(String, String)> {
    let mut names = Vec::new();
    for line in read("docs/roadmap.md").lines() {
        let Some(heading) = line
            .strip_prefix("- [x] **")
            .or_else(|| line.strip_prefix("- [ ] **"))
        else {
            continue;
        };
        let Some((versions, rest)) = heading.split_once(": ") else {
            continue;
        };
        let name = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap(),
            None => rest.split(['.', '*']).next().unwrap(),
        };
        for version in versions.split(" and ") {
            names.push((version.to_string(), name.trim().to_lowercase()));
        }
    }
    names
}

/// The landing page's release cards use the roadmap's names and link to
/// something real (D37): no "MVP", no "MDE", no arrow that goes nowhere.
#[test]
fn release_cards_use_roadmap_release_names() {
    let page = read("docs/website/index.html");
    let roadmap = roadmap_release_names();
    let mut cards = Vec::new();
    for (at, _) in page.match_indices("<aside class=\"status\"") {
        let card = &page[at..at + page[at..].find("</aside>").unwrap()];
        let strong = &card[card.find("<strong>").unwrap() + "<strong>".len()..];
        let strong = decode(&strong[..strong.find("</strong>").unwrap()]);
        let (version, name) = strong
            .split_once(": ")
            .unwrap_or_else(|| panic!("card {strong:?} isn't \"VERSION: name\""));
        assert!(
            roadmap
                .iter()
                .any(|(v, n)| v == version && *n == name.to_lowercase()),
            "card {strong:?} doesn't match a release heading in docs/roadmap.md"
        );
        assert!(
            card.contains("<a href=\"https://"),
            "card {strong:?} links nowhere"
        );
        assert_eq!(
            card.matches('↗').count(),
            card.matches("<span aria-hidden=\"true\">↗</span></a>")
                .count(),
            "card {strong:?} has an arrow outside a link"
        );
        cards.push(version.to_string());
    }
    for release in ["0.2.0", "0.3.0", "0.4.0"] {
        assert!(
            cards.iter().any(|card| card == release),
            "no {release} card"
        );
    }
    for stale in ["(MVP)", "(MDE)", "0.7.0"] {
        assert!(!page.contains(stale), "the page still says {stale:?}");
    }
}

/// Every recording under `assets/` is one the page plays, and every one it
/// plays is there: an orphaned cast is a stale demo nobody checks.
#[test]
fn the_site_references_every_published_cast() {
    let page = read("docs/website/index.html");
    let mut on_disk: Vec<String> = std::fs::read_dir(repository().join("docs/website/assets"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".cast"))
        .collect();
    on_disk.sort();
    let mut played: Vec<String> = page
        .split("data-cast=\"./assets/")
        .skip(1)
        .map(|rest| rest[..rest.find('"').unwrap()].to_string())
        .collect();
    played.sort();
    assert_eq!(on_disk, played);
}

/// The line the Pages workflow uses to find the release the installer
/// fetches, and the ones that take the demo's files from that release.
const DEMO_TAG_LINE: &str =
    "tag=$(sed -n 's/.*RELIABURGER_VERSION:-\\(v[0-9][0-9.]*\\)}.*/\\1/p' docs/website/install.sh)";
const DEMO_FETCH_LINE: &str = "git fetch --depth=1 origin \"refs/tags/$tag:refs/tags/$tag\"";
const DEMO_CHECKOUT_LINE: &str =
    "git checkout \"$tag\" -- examples/kubernetes/podinfo.yaml examples/demo/burger";

/// `/demo/*` is what someone who just installed the release runs, so the
/// workflow publishes it from that release's tag, not from `main` (#688).
#[test]
fn the_demo_assets_come_from_the_installed_release() {
    let workflow = read(".github/workflows/static.yml");
    let tag = workflow.find(DEMO_TAG_LINE).expect("no tag line");
    let fetch = workflow.find(DEMO_FETCH_LINE).expect("no tag fetch");
    let remove = workflow
        .find("rm -rf examples/demo/burger")
        .expect("burger/ isn't cleared before the checkout");
    let checkout = workflow.find(DEMO_CHECKOUT_LINE).expect("no tag checkout");
    let copy = workflow.find(&format!("cp {DEMO_SOURCE}")).unwrap();
    let pack = workflow.find(BURGER_PACK).unwrap();
    assert!(tag < fetch && fetch < remove && remove < checkout);
    assert!(checkout < copy && checkout < pack);

    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("{DEMO_TAG_LINE}; printf %s \"$tag\""))
        .current_dir(repository())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), installer_default());
}

/// The page's `release-testing` section, from its opening tag to its end.
fn release_testing_section(page: &str) -> &str {
    let start = page
        .find("<section id=\"release-testing\"")
        .expect("the page has no release-testing section");
    let end = start + page[start..].find("</section>").unwrap();
    &page[start..end]
}

/// Every `X.Y.Z` version a text names, as numbers.
fn versions_named(text: &str) -> Vec<(u64, u64, u64)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter_map(|word| {
            let parts: Vec<&str> = word.trim_matches('.').split('.').collect();
            match parts.as_slice() {
                [major, minor, patch] => Some((
                    major.parse().ok()?,
                    minor.parse().ok()?,
                    patch.parse().ok()?,
                )),
                _ => None,
            }
        })
        .collect()
}

/// "How a release is tested" follows the jobs section, shows the newest
/// release's record and names no release older than the one the installer
/// fetches, so a promotion can't leave it describing the last one (#979).
#[test]
fn the_release_testing_section_shows_the_installed_release() {
    let page = read("docs/website/index.html");
    let ids: Vec<String> = section_ids(&page).into_iter().flatten().collect();
    let position = |id: &str| ids.iter().position(|i| i == id);
    assert_eq!(
        position("release-testing"),
        position("job-throughput").map(|jobs| jobs + 1),
        "the release-testing section should come straight after the jobs section"
    );
    let section = release_testing_section(&page);
    let installed = installer_default();
    let current = versions_named(&installed)[0];
    let named = versions_named(&strip_tags(section));
    assert!(
        named.contains(&current),
        "the release-testing section doesn't name {installed}"
    );
    for version in named {
        assert!(
            version >= current,
            "the release-testing section names {version:?}, older than {installed}; \
             refresh it (docs/releasing.md, step 9)"
        );
    }
    let log = read("docs/release-log.md");
    let newest = log
        .lines()
        .find(|line| line.starts_with("## "))
        .expect("the release log has no entries");
    assert!(
        newest.starts_with(&format!("## {}:", installed.trim_start_matches('v'))),
        "the release log's newest entry is {newest:?}, not {installed}"
    );
    for link in ["docs/release-log.md", "docs/testing.md#before-a-release"] {
        assert!(
            section.contains(link),
            "the release-testing section doesn't link {link}"
        );
    }
}

/// Every `<img>` tag in `html`, from `<img` to its closing `>`.
fn img_tags(html: &str) -> Vec<&str> {
    html.match_indices("<img")
        .map(|(start, _)| {
            let end = start + html[start..].find('>').unwrap();
            &html[start..=end]
        })
        .collect()
}

/// The value of `name="…"` in one tag, if the tag has it.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(" {name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let end = start + tag[start..].find('"')?;
    Some(&tag[start..end])
}

/// Where the homepage keeps its copies of the soak records' charts.
const SOAK_CHARTS: &str = "docs/website/assets/soak";

/// The release-testing section shows the installed release's soak charts:
/// memory for an app soak and memory plus job rate for a job soak, each a
/// byte-for-byte copy of a chart in a committed qualification record, each
/// linked to that record, and nothing left over from an older release
/// (#979). The Pages workflow publishes only `docs/website/`, so the page
/// can't point into `docs/qualification/` directly.
#[test]
fn the_release_testing_section_shows_the_installed_release_s_soak_charts() {
    let page = read("docs/website/index.html");
    let section = release_testing_section(&page);
    assert!(
        !section.contains("TODO(#979)"),
        "the release-testing section still has the charts placeholder"
    );
    let installed = installer_default();
    let mut shown = Vec::new();
    for tag in img_tags(section) {
        let source = attribute(tag, "src").expect("a chart has no src");
        let Some(chart) = source.strip_prefix("assets/soak/") else {
            continue;
        };
        let (directory, file) = chart
            .split_once('/')
            .unwrap_or_else(|| panic!("{source} isn't assets/soak/<record>-charts/<chart>.svg"));
        let record = directory
            .strip_suffix("-charts")
            .unwrap_or_else(|| panic!("{source} isn't in a <record>-charts directory"));
        assert!(
            record.contains(&format!("-{installed}-")),
            "{source} comes from another release's record, not {installed}'s; \
             refresh the charts (docs/releasing.md, step 9)"
        );
        let copy = std::fs::read(repository().join("docs/website").join(source))
            .unwrap_or_else(|error| panic!("{source} isn't in docs/website: {error}"));
        let original = std::fs::read(
            repository()
                .join("docs/qualification")
                .join(directory)
                .join(file),
        )
        .unwrap_or_else(|error| panic!("{source} has no original in docs/qualification: {error}"));
        assert!(
            copy == original,
            "{source} differs from docs/qualification/{directory}/{file}; copy it again"
        );
        assert!(
            attribute(tag, "alt").is_some_and(|alt| !alt.trim().is_empty()),
            "{source} has no alt text"
        );
        for size in ["width", "height"] {
            assert!(
                attribute(tag, size).is_some(),
                "{source} has no {size}, so the page jumps as it loads"
            );
        }
        assert_eq!(
            attribute(tag, "loading"),
            Some("lazy"),
            "{source} should load only when the results are opened"
        );
        assert!(
            section.contains(&format!("docs/qualification/{record}.md")),
            "the release-testing section doesn't link {record}.md, the record {source} comes from"
        );
        shown.push(format!("{directory}/{file}"));
    }
    for (kind, chart) in [("app soak", "sustained"), ("job soak", "job-qualification")] {
        for file in ["rss.svg", "fds.svg", "cpu.svg"] {
            assert!(
                shown.iter().any(|s| s.contains(chart) && s.ends_with(file)),
                "the release-testing section shows no {file} from the {kind}"
            );
        }
    }
    assert!(
        shown
            .iter()
            .any(|s| s.contains("job-qualification") && s.ends_with("jobs.svg")),
        "the release-testing section shows no job-rate chart from the job soak"
    );
    let mut on_disk = Vec::new();
    for directory in std::fs::read_dir(repository().join(SOAK_CHARTS)).unwrap() {
        let directory = directory.unwrap().path();
        for file in std::fs::read_dir(&directory).unwrap() {
            let file = file.unwrap().path();
            on_disk.push(format!(
                "{}/{}",
                directory.file_name().unwrap().to_string_lossy(),
                file.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    on_disk.sort();
    shown.sort();
    assert_eq!(
        on_disk, shown,
        "{SOAK_CHARTS} should hold exactly the charts the page shows; remove the old release's"
    );
}

/// The charts are 720 px wide, wider than a phone: they scale down with the
/// column instead of pushing the page sideways (#979).
#[test]
fn soak_charts_shrink_to_fit_a_phone() {
    let style = read("docs/website/style.css");
    let rule = style
        .lines()
        .find(|line| line.starts_with(".soak-chart img"))
        .expect("style.css has no .soak-chart img rule");
    for declaration in ["max-width: 100%", "height: auto"] {
        assert!(
            rule.contains(declaration),
            ".soak-chart img should have {declaration}: {rule}"
        );
    }
}
