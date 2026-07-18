//! Markdown to ratatui lines.
//!
//! One pulldown-cmark parse feeds both faces of the manual: this module
//! styles the event stream for the terminal, `web.rs` turns the same
//! markdown into HTML. No second markdown dialect to drift.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render markdown into styled terminal lines.
pub fn markdown_to_lines(source: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let mut builder = Builder::default();
    for event in Parser::new_ext(source, Options::empty()) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                builder.flush();
                builder.blank_before_block();
                builder.heading = Some(if level == pulldown_cmark::HeadingLevel::H1 {
                    1
                } else {
                    2
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                builder.flush();
                builder.heading = None;
                builder.blank();
            }
            Event::End(TagEnd::Paragraph) => {
                builder.flush();
                builder.blank();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                builder.flush();
                builder.code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                builder.code_block = false;
                builder.blank();
            }
            Event::Start(Tag::List(start)) => {
                builder.flush();
                builder.list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                builder.list_stack.pop();
                if builder.list_stack.is_empty() {
                    builder.blank();
                }
            }
            Event::Start(Tag::Item) => {
                builder.flush();
                let indent = "  ".repeat(builder.list_stack.len().saturating_sub(1));
                let marker = match builder.list_stack.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "- ".to_string(),
                };
                builder.current.push(Span::raw(format!("{indent}{marker}")));
            }
            Event::End(TagEnd::Item) => builder.flush(),
            Event::Start(Tag::BlockQuote(_)) => {
                builder.flush();
                builder.quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                builder.quote_depth = builder.quote_depth.saturating_sub(1);
                builder.blank();
            }
            Event::Start(Tag::Strong) => builder.strong = true,
            Event::End(TagEnd::Strong) => builder.strong = false,
            Event::Start(Tag::Emphasis) => builder.emphasis = true,
            Event::End(TagEnd::Emphasis) => builder.emphasis = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                builder.link = Some(dest_url.into_string());
                builder.link_text.clear();
            }
            Event::End(TagEnd::Link) => builder.end_link(),
            Event::Text(text) => builder.text(&text),
            Event::Code(code) => builder.current.push(Span::styled(
                code.into_string(),
                Style::default().fg(INLINE_CODE),
            )),
            Event::SoftBreak | Event::HardBreak => builder.flush(),
            Event::Rule => {
                builder.flush();
                builder
                    .lines
                    .push(Line::styled("─".repeat(40), Style::default().fg(MUTED)));
                builder.blank();
            }
            _ => {}
        }
    }
    builder.flush();
    // No trailing blank padding.
    while builder
        .lines
        .last()
        .is_some_and(|line| line.spans.is_empty())
    {
        builder.lines.pop();
    }
    builder.lines
}

const HEADING: Color = Color::Rgb(244, 162, 97); // matches the TUI accent
const INLINE_CODE: Color = Color::Rgb(82, 183, 136);
const CODE_BLOCK: Color = Color::Rgb(82, 183, 136);
const MUTED: Color = Color::DarkGray;

#[derive(Default)]
struct Builder {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    /// Heading level while inside one (1 = `#`).
    heading: Option<usize>,
    code_block: bool,
    quote_depth: usize,
    /// Ordered-list counters; `None` entries are bullet lists.
    list_stack: Vec<Option<u64>>,
    strong: bool,
    emphasis: bool,
    link: Option<String>,
    link_text: String,
}

impl Builder {
    fn text(&mut self, text: &str) {
        if self.code_block {
            for line in text.lines() {
                self.lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(CODE_BLOCK),
                )));
            }
            return;
        }
        if self.link.is_some() {
            self.link_text.push_str(text);
        }
        let style = self.text_style();
        self.current.push(Span::styled(text.to_string(), style));
    }

    fn text_style(&self) -> Style {
        if let Some(level) = self.heading {
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if level == 1 {
                style = style.fg(HEADING);
            }
            return style;
        }
        let mut style = Style::default();
        if self.strong {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.link.is_some() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    /// Close a link: append its URL unless the text already was the URL
    /// (autolinks like `<http://…>`).
    fn end_link(&mut self) {
        if let Some(url) = self.link.take()
            && self.link_text != url
            && !url.is_empty()
        {
            self.current.push(Span::styled(
                format!(" ({url})"),
                Style::default().fg(MUTED),
            ));
        }
    }

    /// Emit the accumulated spans as one line (quote-prefixed if needed).
    fn flush(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let mut spans = Vec::with_capacity(self.current.len() + 1);
        if self.quote_depth > 0 {
            spans.push(Span::styled(
                "> ".repeat(self.quote_depth),
                Style::default().fg(MUTED),
            ));
        }
        spans.append(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    /// One blank separator line (never two in a row, never as line one).
    fn blank(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn blank_before_block(&mut self) {
        if !self.lines.is_empty() {
            self.blank();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::reader::line_text;

    const FIXTURE: &str = "# Title\n\nIntro with `inline code` here.\n\n## Section\n\n- first item\n- second item\n\n```sh\nrelish status\nrelish top\n```\n\n> a quote\n\n[the book](https://example.com/book)\n";

    fn rendered() -> Vec<Line<'static>> {
        markdown_to_lines(FIXTURE)
    }

    #[test]
    fn renders_the_fixture_layout() {
        let text: Vec<String> = rendered().iter().map(line_text).collect();
        insta::assert_snapshot!(text.join("\n"));
    }

    #[test]
    fn headings_are_bold() {
        let lines = rendered();
        let title = lines
            .iter()
            .find(|line| line_text(line) == "Title")
            .expect("h1 present");
        assert!(title.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn code_blocks_are_indented_and_coloured() {
        let lines = rendered();
        let code = lines
            .iter()
            .find(|line| line_text(line).contains("relish status"))
            .expect("code line present");
        assert!(line_text(code).starts_with("  "));
        assert!(code.spans.iter().any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn inline_code_is_styled_within_its_line() {
        let lines = rendered();
        let intro = lines
            .iter()
            .find(|line| line_text(line).contains("inline code"))
            .expect("intro present");
        assert!(intro.spans.len() > 1, "inline code splits the line's spans");
    }

    #[test]
    fn list_items_get_bullets() {
        let text: Vec<String> = rendered().iter().map(line_text).collect();
        assert!(text.iter().any(|line| line.starts_with("- first item")));
    }

    #[test]
    fn links_keep_text_and_show_the_url() {
        let text: Vec<String> = rendered().iter().map(line_text).collect();
        let line = text
            .iter()
            .find(|line| line.contains("the book"))
            .expect("link present");
        assert!(line.contains("https://example.com/book"));
    }

    #[test]
    fn empty_input_renders_no_lines() {
        assert!(markdown_to_lines("").is_empty());
    }
}
