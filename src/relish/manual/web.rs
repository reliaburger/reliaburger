//! `relish manual --web` — the whole manual as one self-contained HTML page.
//!
//! pulldown-cmark renders each chapter to HTML; the page inlines its CSS and
//! a small client-side filter box. Served from loopback by axum and opened
//! in the default browser.

use axum::Router;
use axum::response::Html;
use axum::routing::get;

use super::assets::{Chapter, chapters};
use crate::relish::RelishError;

/// Build the complete manual page: table of contents, a filter box, one
/// section per chapter.
pub fn build_page(chapters: &[Chapter]) -> String {
    use std::fmt::Write as _;

    let mut toc = String::new();
    let mut sections = String::new();
    for (index, chapter) in chapters.iter().enumerate() {
        let anchor = format!("ch{index}");
        let title = escape_html(&chapter.title);
        // Writing to a String cannot fail.
        writeln!(toc, "<li><a href=\"#{anchor}\">{title}</a></li>").unwrap();
        let mut body = String::new();
        pulldown_cmark::html::push_html(
            &mut body,
            pulldown_cmark::Parser::new_ext(&chapter.markdown, pulldown_cmark::Options::empty()),
        );
        writeln!(sections, "<section id=\"{anchor}\">\n{body}</section>").unwrap();
    }

    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Reliaburger manual</title>\n<style>{CSS}</style>\n</head>\n<body>\n\
         <header><h1 class=\"site\">Reliaburger manual</h1>\n\
         <input id=\"filter\" type=\"search\" placeholder=\"filter chapters…\" autofocus></header>\n\
         <nav><ul>\n{toc}</ul></nav>\n<main>\n{sections}</main>\n\
         <script>{FILTER_JS}</script>\n</body>\n</html>\n"
    )
}

const CSS: &str = "\
body{font-family:system-ui,sans-serif;line-height:1.55;max-width:46rem;\
margin:0 auto;padding:1rem 1.25rem 4rem;color:#1c1c1c;background:#fffdf8}\
h1{border-bottom:2px solid #f4a261;padding-bottom:.2rem}\
h1.site{border:none;margin-bottom:.25rem}\
code{background:#f2ece1;padding:.1rem .3rem;border-radius:4px;\
font-size:.92em}\
pre{background:#2b2b2b;color:#eee;padding:.75rem 1rem;border-radius:8px;\
overflow-x:auto}pre code{background:none;color:inherit;padding:0}\
nav ul{list-style:none;padding:0;display:flex;flex-wrap:wrap;gap:.25rem 1rem}\
nav a{text-decoration:none;color:#b35c1e}\
#filter{width:100%;padding:.5rem .75rem;font-size:1rem;border:1px solid #ccc;\
border-radius:8px;box-sizing:border-box}\
section{border-bottom:1px dashed #ddd;padding-bottom:1rem}\
a{color:#b35c1e}table{border-collapse:collapse}\
td,th{border:1px solid #ddd;padding:.25rem .5rem}";

const FILTER_JS: &str = "\
const filter=document.getElementById('filter');\
filter.addEventListener('input',()=>{\
const q=filter.value.toLowerCase();\
document.querySelectorAll('main section').forEach(s=>{\
s.style.display=s.textContent.toLowerCase().includes(q)?'':'none';});});";

/// Minimal HTML escaping for text we interpolate outside markdown.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A router serving the page at `/`.
pub fn router(page: String) -> Router {
    Router::new().route("/", get(move || async move { Html(page) }))
}

/// Serve the manual on loopback and open the browser. Runs until Ctrl-C.
pub async fn serve(port: u16) -> Result<(), RelishError> {
    let page = build_page(&chapters());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let url = format!("http://{}/", listener.local_addr()?);
    println!("manual at {url} (Ctrl-C to stop)");
    open_browser(&url);
    axum::serve(listener, router(page)).await?;
    Ok(())
}

/// Best-effort browser launch; the printed URL is the fallback.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(not(target_os = "macos"))]
    let command = "xdg-open";
    let _ = std::process::Command::new(command)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tower::util::ServiceExt as _;

    #[test]
    fn page_contains_every_chapter() {
        let chapters = chapters();
        let page = build_page(&chapters);
        assert!(page.starts_with("<!doctype html>"));
        for chapter in &chapters {
            assert!(
                page.contains(&chapter.title),
                "page is missing chapter {}",
                chapter.file
            );
        }
        // Markdown actually rendered: a known command ends up inside HTML.
        assert!(page.contains("relish setup"));
        assert!(page.contains("<h1"));
        // Self-contained: inline style and the filter box are present.
        assert!(page.contains("<style>"));
        assert!(page.contains("id=\"filter\""));
    }

    #[test]
    fn titles_are_html_escaped() {
        let chapter = Chapter {
            file: "99_x.md".to_string(),
            title: "a <b> & c".to_string(),
            markdown: "# a <b> & c\n".to_string(),
        };
        let page = build_page(std::slice::from_ref(&chapter));
        assert!(page.contains("a &lt;b&gt; &amp; c"));
    }

    #[tokio::test]
    async fn router_serves_the_page_as_html() {
        let response = router("<!doctype html><p>hi</p>".to_string())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.starts_with("text/html"));
    }
}
