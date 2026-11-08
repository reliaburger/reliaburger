//! Every website page has a Markdown copy beside it (#1582).
//!
//! An agent reading `https://reliaburger.com/compare/nomad.html` gets a page
//! built for eyes: navigation, inline SVG, buttons, a carousel. Beside each
//! page the site serves the same words as Markdown, at the same path with
//! `.md` in place of `.html` (`index.md` for `/`), and each page names its
//! copy with `<link rel="alternate" type="text/markdown">`. `llms.txt` lists
//! them all.
//!
//! The pages are hand-written HTML, so the copies are generated from them:
//! [`markdown_copy`] parses a page, keeps its `<main>`, drops what only
//! makes sense on screen and converts the rest with `htmd`. The test below
//! fails while a copy differs from what its page makes, so a page edit that
//! skipped `make agent-docs` can't ship a stale copy. `agents.md` is the
//! exception: `relish agent guide`'s generator writes it, and `agents.html`,
//! straight from the manual chapter, and `agent_docs` checks both.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use markup5ever_rcdom::{Node, NodeData};
use reliaburger::relish::agent::guide::{self, REGENERATE_COMMAND, SitePage};

use super::website::{read, repository};

/// Where the site is published.
const SITE: &str = "https://reliaburger.com/";

/// The website's source directory, relative to the repository root.
const WEBSITE: &str = "docs/website";

/// Elements a reader of the Markdown has no use for: chrome, drawings,
/// controls and scripts. The page's own text lives in `<main>`.
const DROPPED_ELEMENTS: &[&str] = &[
    "script", "style", "svg", "noscript", "button", "nav", "template", "input", "select",
    "textarea",
];

/// Classes whose elements are on-screen furniture: the `#` permalink after
/// every heading.
const DROPPED_CLASSES: &[&str] = &["permalink", "skip"];

/// Every HTML page under `docs/website`, relative to it, sorted. The vendored
/// player and the demo files aren't pages.
pub(crate) fn pages() -> Vec<String> {
    let root = repository().join(WEBSITE);
    let mut found = Vec::new();
    collect_pages(&root, &root, &mut found);
    // The homepage first, then the rest in path order.
    found.sort_by_key(|page| (page != "index.html", page.clone()));
    found
}

fn collect_pages(root: &Path, directory: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            if relative != "assets" && relative != "demo" {
                collect_pages(root, &path, found);
            }
        } else if relative.ends_with(".html") {
            found.push(relative);
        }
    }
}

/// The copy's path beside `page`: `compare/nomad.html` → `compare/nomad.md`.
pub(crate) fn copy_path(page: &str) -> String {
    format!("{}.md", page.trim_end_matches(".html"))
}

/// Where `page` is published, which its relative links resolve against.
fn page_url(page: &str) -> url::Url {
    let base = url::Url::parse(SITE).unwrap();
    let path = page.strip_suffix("index.html").unwrap_or(page);
    base.join(path).unwrap()
}

/// The text of `page`'s `<title>`.
fn title(html: &str) -> String {
    let start = html.find("<title>").unwrap() + "<title>".len();
    let end = html[start..].find("</title>").unwrap() + start;
    html[start..end]
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

/// Every page's Markdown copy, for `llms.txt`.
pub(crate) fn site_pages() -> Vec<SitePage> {
    pages()
        .into_iter()
        .map(|page| SitePage {
            title: title(&read(&format!("{WEBSITE}/{page}"))),
            markdown: copy_path(&page),
        })
        .collect()
}

fn element_name(node: &Node) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(&name.local),
        _ => None,
    }
}

fn attribute(node: &Node, wanted: &str) -> Option<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return None;
    };
    attrs
        .borrow()
        .iter()
        .find(|attribute| &*attribute.name.local == wanted)
        .map(|attribute| attribute.value.to_string())
}

/// Whether `node` is screen furniture to leave out of the copy.
fn is_dropped(node: &Node) -> bool {
    let Some(name) = element_name(node) else {
        return false;
    };
    if DROPPED_ELEMENTS.contains(&name) {
        return true;
    }
    if attribute(node, "aria-hidden").as_deref() == Some("true") {
        return true;
    }
    attribute(node, "class").is_some_and(|classes| {
        classes
            .split_whitespace()
            .any(|class| DROPPED_CLASSES.contains(&class))
    })
}

/// Drop the furniture under `node` and make every link and image absolute
/// against `base`. Two shapes htmd can't take are reshaped first: a `<br>`
/// inside a heading becomes a space, since a Markdown heading is one line,
/// and a row header (`<th scope="row">`) in a table body becomes a `<td>`,
/// since htmd reads only `<td>` cells from body rows and would drop it.
fn prune(node: &Rc<Node>, base: &url::Url, in_heading: bool) {
    let name = element_name(node);
    let in_heading = in_heading
        || name.is_some_and(|name| {
            name.len() == 2 && name.starts_with('h') && name[1..].parse::<u8>().is_ok()
        });
    let body_row = name == Some("tr") && !is_header_row(node);
    node.children
        .borrow_mut()
        .retain(|child| !is_dropped(child));
    // Clone the list (cheap: it copies `Rc` pointers) so no borrow of
    // `children` is open while the loop replaces entries or recurses.
    let children: Vec<Rc<Node>> = node.children.borrow().clone();
    for (position, child) in children.iter().enumerate() {
        if in_heading && element_name(child) == Some("br") {
            let space = Node::new(NodeData::Text {
                contents: RefCell::new(" ".into()),
            });
            node.children.borrow_mut()[position] = space;
            continue;
        }
        let child = if body_row && element_name(child) == Some("th") {
            let cell = renamed(child, "td");
            node.children.borrow_mut()[position] = cell.clone();
            cell
        } else {
            child.clone()
        };
        absolutise(&child, base);
        prune(&child, base, in_heading);
    }
}

/// Whether the row `tr` sits in a `<thead>`.
fn is_header_row(tr: &Node) -> bool {
    let parent = tr.parent.take();
    let in_head = parent
        .as_ref()
        .and_then(|weak| weak.upgrade())
        .is_some_and(|parent| element_name(&parent) == Some("thead"));
    tr.parent.set(parent);
    in_head
}

/// A copy of the element `node` called `tag`, with its attributes and
/// children.
fn renamed(node: &Rc<Node>, tag: &str) -> Rc<Node> {
    let NodeData::Element {
        name,
        attrs,
        template_contents,
        mathml_annotation_xml_integration_point,
    } = &node.data
    else {
        return node.clone();
    };
    let mut name = name.clone();
    name.local = tag.into();
    let copy = Node::new(NodeData::Element {
        name,
        attrs: RefCell::new(attrs.borrow().clone()),
        template_contents: RefCell::new(template_contents.borrow().clone()),
        mathml_annotation_xml_integration_point: *mathml_annotation_xml_integration_point,
    });
    copy.children
        .replace(node.children.borrow().iter().cloned().collect());
    copy
}

/// `markdown` without trailing spaces, outside code blocks, where htmd
/// leaves them after inline elements.
fn without_trailing_spaces(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        out.push_str(if in_fence { line } else { line.trim_end() });
        out.push('\n');
    }
    out
}

fn absolutise(node: &Node, base: &url::Url) {
    let NodeData::Element { attrs, .. } = &node.data else {
        return;
    };
    for attribute in attrs.borrow_mut().iter_mut() {
        if !matches!(&*attribute.name.local, "href" | "src") {
            continue;
        }
        if let Ok(absolute) = base.join(&attribute.value) {
            attribute.value = absolute.as_str().into();
        }
    }
}

fn find_main(node: &Rc<Node>) -> Option<Rc<Node>> {
    if element_name(node) == Some("main") {
        return Some(node.clone());
    }
    node.children.borrow().iter().find_map(find_main)
}

/// The Markdown copy of `page` (a path under `docs/website`), whose HTML is
/// `html`.
pub(crate) fn markdown_copy(page: &str, html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .options(htmd::options::Options {
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            br_style: htmd::options::BrStyle::Backslash,
            ..Default::default()
        })
        .build();
    let document = converter.html_to_tree(html).unwrap();
    let main = find_main(&document).unwrap_or_else(|| panic!("{page} has no <main>"));
    let url = page_url(page);
    prune(&main, &url, false);
    let markdown = without_trailing_spaces(&converter.tree_to_markdown(&main));
    let body = markdown.trim_end();
    format!(
        "<!-- Generated from {WEBSITE}/{page} by `{REGENERATE_COMMAND}`: edit the page, not this file. -->\n\n\
         {body}\n\n---\n\nThe Markdown copy of <{url}>.\n"
    )
}

/// The pages whose copy this module writes: every page but those the agent
/// chapter's generator makes.
fn converted_pages() -> Vec<String> {
    let generated = Path::new(guide::paths::AGENTS_PAGE_HTML)
        .strip_prefix(WEBSITE)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    pages()
        .into_iter()
        .filter(|page| *page != generated)
        .collect()
}

fn copy_on_disk(page: &str) -> PathBuf {
    repository().join(WEBSITE).join(copy_path(page))
}

/// The first few lines where a page's fresh copy and the copy on disk
/// differ, so a failure says what changed even where `make agent-docs`
/// can't run.
fn differences(fresh: &str, on_disk: &str) -> String {
    let mut fresh_lines = fresh.lines();
    let mut disk_lines = on_disk.lines();
    let mut found = Vec::new();
    let mut number = 1;
    while found.len() < 3 {
        let (expected, actual) = (fresh_lines.next(), disk_lines.next());
        if expected.is_none() && actual.is_none() {
            break;
        }
        if expected != actual {
            found.push(format!(
                "line {number}: the page makes {:?}, the file has {:?}",
                expected.unwrap_or("<end>"),
                actual.unwrap_or("<end>")
            ));
        }
        number += 1;
    }
    if found.is_empty() {
        return "they differ only in line endings".to_string();
    }
    found.join("; ")
}

#[test]
fn every_page_has_a_current_markdown_copy() {
    let update = std::env::var_os("RELIABURGER_UPDATE_AGENT_DOCS").is_some();
    let mut stale = Vec::new();
    for page in converted_pages() {
        let fresh = markdown_copy(&page, &read(&format!("{WEBSITE}/{page}")));
        let path = copy_on_disk(&page);
        if update {
            std::fs::write(&path, &fresh).unwrap();
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(on_disk) if on_disk == fresh => {}
            Ok(on_disk) => {
                let difference = differences(&fresh, &on_disk);
                stale.push(format!("{} ({difference})", copy_path(&page)));
            }
            Err(_) => stale.push(format!("{} (missing)", copy_path(&page))),
        }
    }
    assert!(
        stale.is_empty(),
        "out of date with their pages: {stale:?}; run `{REGENERATE_COMMAND}`"
    );
}

#[test]
fn every_page_names_its_markdown_copy() {
    for page in pages() {
        let html = read(&format!("{WEBSITE}/{page}"));
        let file = Path::new(&copy_path(&page))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let link = format!(r#"<link rel="alternate" type="text/markdown" href="{file}">"#);
        assert!(html.contains(&link), "{page} lacks {link}");
        assert!(copy_on_disk(&page).is_file(), "{page} has no copy");
    }
}

#[test]
fn no_markdown_copy_outlives_its_page() {
    let root = repository().join(WEBSITE);
    let copies: Vec<String> = pages().iter().map(|page| copy_path(page)).collect();
    let mut found = Vec::new();
    collect_markdown(&root, &root, &mut found);
    for copy in found {
        assert!(
            copies.contains(&copy),
            "{WEBSITE}/{copy} has no page beside it; delete it or add the page"
        );
    }
}

/// Markdown files under `docs/website`, other than its README and the
/// skills directory, which holds a skill rather than a page's copy.
fn collect_markdown(root: &Path, directory: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            if !matches!(relative.as_str(), "assets" | "demo" | ".well-known") {
                collect_markdown(root, &path, found);
            }
        } else if relative.ends_with(".md") && relative != "README.md" {
            found.push(relative);
        }
    }
}

#[test]
fn llms_txt_lists_every_markdown_copy() {
    let llms = read(guide::paths::LLMS_TXT);
    for page in site_pages() {
        let link = format!("]({SITE}{})", page.markdown);
        assert!(
            llms.contains(&link),
            "llms.txt doesn't link {}",
            page.markdown
        );
    }
}

#[test]
fn a_copy_keeps_the_words_and_drops_the_furniture() {
    let html = r##"<!doctype html><html><head><title>T</title><script>x()</script></head>
<body><a class="skip" href="#main">Skip to content</a>
<header class="masthead"><nav><a href="#install">Install</a></nav></header>
<main id="main"><h1 id="title">The platform,<br>served ready.</h1>
<p>Read <a href="./compare/">the comparisons</a> or <a href="#install">install</a>.</p>
<h2 id="install">Install <a class="permalink" href="#install" aria-label="Link">#</a></h2>
<svg><text>drawing</text></svg><span aria-hidden="true">↗</span>
<button type="button">Copy</button>
<pre><code>curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop</code></pre>
<img src="assets/chart.svg" alt="Memory"></main>
<footer>Static HTML</footer></body></html>"##;
    let copy = markdown_copy("index.html", html);
    assert!(copy.contains("# The platform, served ready."), "{copy}");
    assert!(copy.contains("[the comparisons](https://reliaburger.com/compare/)"));
    assert!(copy.contains("[install](https://reliaburger.com/#install)"));
    assert!(copy.contains("## Install\n"), "{copy}");
    assert!(copy.contains("```\ncurl -fsSL https://reliaburger.com/install.sh"));
    assert!(copy.contains("![Memory](https://reliaburger.com/assets/chart.svg)"));
    for gone in [
        "Skip to content",
        "drawing",
        "↗",
        "Copy",
        "Static HTML",
        "x()",
    ] {
        assert!(!copy.contains(gone), "{gone:?} survived: {copy}");
    }
}

#[test]
fn links_in_a_subdirectory_resolve_against_that_page() {
    let html = r#"<main><p><a href="../#install">Install</a> <a href="./methodology.html#method">method</a></p></main>"#;
    let copy = markdown_copy("compare/nomad.html", html);
    assert!(
        copy.contains("(https://reliaburger.com/#install)"),
        "{copy}"
    );
    assert!(copy.contains("(https://reliaburger.com/compare/methodology.html#method)"));
    assert!(copy.contains("<https://reliaburger.com/compare/nomad.html>"));
}

#[test]
fn a_row_header_keeps_its_cell() {
    let html = r#"<main><table><thead><tr><th>Need</th><th>Nomad</th></tr></thead>
<tbody><tr><th scope="row">An app</th><td>A job</td></tr></tbody></table></main>"#;
    let copy = markdown_copy("compare/nomad.html", html);
    assert!(copy.contains("| Need"), "{copy}");
    let row = copy
        .lines()
        .find(|line| line.contains("A job"))
        .unwrap_or_else(|| panic!("{copy}"));
    assert!(row.contains("An app"), "the row header went missing: {row}");
}

#[test]
fn a_copy_has_no_trailing_spaces_outside_code() {
    for page in converted_pages() {
        let copy = std::fs::read_to_string(copy_on_disk(&page)).unwrap();
        let mut in_fence = false;
        for line in copy.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
            }
            assert!(
                in_fence || !line.ends_with(' '),
                "{}: {line:?}",
                copy_path(&page)
            );
        }
    }
}
