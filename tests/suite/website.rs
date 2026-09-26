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
    "burger.toml",
    "go.mod",
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
