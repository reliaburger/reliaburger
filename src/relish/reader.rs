//! Shared document reader TUI: a chapter list, a scrollable content pane
//! and nucleo-backed fuzzy search.
//!
//! `relish manual` reads rendered markdown chapters through it and
//! `relish source` reads embedded source files; the reader itself only
//! knows about titled documents made of ratatui [`Line`]s. The state
//! machine ([`ReaderState`]) is pure and keyboard-tested; the async
//! [`run`] loop owns the terminal.

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use futures_util::StreamExt as _;
use ratatui::Frame;
use ratatui::text::Line;

use super::RelishError;
use super::tui::terminal::TerminalGuard;

/// How many rows PageUp/PageDown move.
const PAGE: usize = 20;

/// Cap on fuzzy results kept per query.
const MAX_RESULTS: usize = 100;

/// One readable document: a title for the list pane and pre-rendered lines
/// for the content pane.
pub struct Document {
    pub title: String,
    pub lines: Vec<Line<'static>>,
}

/// Which pane keyboard navigation drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Content,
}

/// One fuzzy hit: a document, the line within it, and the matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub doc: usize,
    pub line: usize,
    pub preview: String,
}

/// Live search input and its ranked results.
#[derive(Debug, Clone, Default)]
pub struct ReaderSearch {
    pub input: String,
    pub results: Vec<SearchResult>,
    pub cursor: usize,
}

/// Complete reader state; the reducer is [`ReaderState::handle_key`].
pub struct ReaderState {
    pub docs: Vec<Document>,
    /// Title of the list pane ("chapters", "files", ...).
    pub list_title: String,
    pub selected: usize,
    pub scroll: usize,
    pub focus: Focus,
    pub search: Option<ReaderSearch>,
    pub should_quit: bool,
    /// Content rows currently visible; the run loop keeps it current so
    /// scrolling can clamp to the end of the document.
    pub viewport_rows: usize,
}

/// Rank `candidates` against a fuzzy `query`, best first. Returns indices
/// into `candidates`; non-matching candidates are absent.
pub fn fuzzy_rank(query: &str, candidates: &[String]) -> Vec<usize> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut scored: Vec<(usize, u32)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let haystack = Utf32Str::new(candidate, &mut buffer);
            pattern
                .score(haystack, &mut matcher)
                .map(|score| (index, score))
        })
        .collect();
    // Best score first; original order breaks ties so results are stable.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(index, _)| index).collect()
}

impl ReaderState {
    /// Create a reader over `docs`, opened on the first document.
    pub fn new(docs: Vec<Document>) -> Self {
        Self {
            docs,
            list_title: "documents".to_string(),
            selected: 0,
            scroll: 0,
            focus: Focus::List,
            search: None,
            should_quit: false,
            viewport_rows: 24,
        }
    }

    /// Open the search pane with `seed` already typed (and matched).
    /// `relish source ebpf` lands here.
    pub fn open_search(&mut self, seed: &str) {
        self.search = Some(ReaderSearch {
            input: seed.to_string(),
            ..ReaderSearch::default()
        });
        self.rebuild_search();
    }

    /// The keyboard reducer.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.search.is_some() {
            self.handle_search_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('/') | KeyCode::Char('s') => self.open_search(""),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Content,
                    Focus::Content => Focus::List,
                }
            }
            KeyCode::Enter => self.focus = Focus::Content,
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.scroll_by(-(PAGE as isize)),
            KeyCode::PageDown => self.scroll_by(PAGE as isize),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('G') => self.scroll = self.max_scroll(),
            KeyCode::Left => self.select_delta(-1),
            KeyCode::Right => self.select_delta(1),
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => {
                let hit = self
                    .search
                    .as_ref()
                    .and_then(|search| search.results.get(search.cursor))
                    .cloned();
                if let Some(hit) = hit {
                    self.selected = hit.doc;
                    // Put the hit at the top of the pane; short documents may
                    // leave blank rows below, which beats hiding the hit.
                    self.scroll = hit.line;
                    self.focus = Focus::Content;
                    self.search = None;
                }
            }
            KeyCode::Up => {
                if let Some(search) = &mut self.search {
                    search.cursor = search.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(search) = &mut self.search {
                    search.cursor = (search.cursor + 1).min(search.results.len().saturating_sub(1));
                }
            }
            KeyCode::Backspace => {
                if let Some(search) = &mut self.search {
                    search.input.pop();
                }
                self.rebuild_search();
            }
            KeyCode::Char(character) => {
                if let Some(search) = &mut self.search {
                    search.input.push(character);
                }
                self.rebuild_search();
            }
            _ => {}
        }
    }

    fn rebuild_search(&mut self) {
        let all = self.all_lines();
        let texts: Vec<String> = all.iter().map(|(_, _, text)| text.clone()).collect();
        let Some(search) = &mut self.search else {
            return;
        };
        let ranked = fuzzy_rank(&search.input, &texts);
        search.results = ranked
            .into_iter()
            .take(MAX_RESULTS)
            .map(|index| {
                let (doc, line, ref text) = all[index];
                SearchResult {
                    doc,
                    line,
                    preview: text.clone(),
                }
            })
            .collect();
        search.cursor = 0;
    }

    /// Move the selection (list focus) or scroll (content focus).
    fn move_by(&mut self, delta: isize) {
        match self.focus {
            Focus::List => self.select_delta(delta),
            Focus::Content => self.scroll_by(delta),
        }
    }

    fn select_delta(&mut self, delta: isize) {
        if self.docs.is_empty() {
            return;
        }
        let last = self.docs.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.scroll = 0;
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.docs
            .get(self.selected)
            .map(|doc| doc.lines.len().saturating_sub(self.viewport_rows))
            .unwrap_or(0)
    }

    /// Plain text of every line of every document, as `(doc, line, text)`.
    fn all_lines(&self) -> Vec<(usize, usize, String)> {
        self.docs
            .iter()
            .enumerate()
            .flat_map(|(doc, document)| {
                document
                    .lines
                    .iter()
                    .enumerate()
                    .map(move |(line, content)| (doc, line, line_text(content)))
            })
            .filter(|(_, _, text)| !text.trim().is_empty())
            .collect()
    }
}

/// Plain text of one rendered line (spans concatenated).
pub fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the reader: list pane, content pane, optional search overlay.
pub fn render(frame: &mut Frame<'_>, state: &ReaderState) {
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::Style;
    use ratatui::text::Span;
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

    use super::tui::theme;

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(1)])
        .split(outer[0]);

    // List pane.
    let items: Vec<Line<'static>> = state
        .docs
        .iter()
        .enumerate()
        .map(|(index, doc)| {
            let text = format!(" {}", doc.title);
            if index == state.selected {
                Line::styled(text, theme::selected())
            } else {
                Line::raw(text)
            }
        })
        .collect();
    let list_border = if state.focus == Focus::List && state.search.is_none() {
        theme::title()
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(list_border)
                .title(state.list_title.clone()),
        ),
        panes[0],
    );

    // Content pane, or the search results while searching.
    if let Some(search) = &state.search {
        let mut lines = vec![
            Line::styled(format!("> {}", search.input), theme::title()),
            Line::raw(""),
        ];
        if search.input.is_empty() {
            lines.push(Line::raw("type to search"));
        } else if search.results.is_empty() {
            lines.push(Line::raw("no matches"));
        }
        for (index, hit) in search.results.iter().enumerate() {
            let title = state
                .docs
                .get(hit.doc)
                .map(|doc| doc.title.as_str())
                .unwrap_or("?");
            let text = format!("{title:<24} {}", hit.preview.trim());
            lines.push(if index == search.cursor {
                Line::styled(text, theme::selected())
            } else {
                Line::raw(text)
            });
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::title())
                    .title("search"),
            ),
            panes[1],
        );
    } else {
        let empty = Vec::new();
        let (title, lines) = state
            .docs
            .get(state.selected)
            .map(|doc| (doc.title.as_str(), &doc.lines))
            .unwrap_or(("empty", &empty));
        let content_border = if state.focus == Focus::Content {
            theme::title()
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(lines.clone())
                .scroll((state.scroll.min(u16::MAX as usize) as u16, 0))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(content_border)
                        .title(title.to_string()),
                ),
            panes[1],
        );
    }

    // Status bar.
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(
            " [/]search [Tab]pane [↑↓]move [←→]document [PgUp/PgDn]scroll [q]quit",
        )])),
        outer[1],
    );
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Run the reader until the user quits.
pub async fn run(
    list_title: &str,
    docs: Vec<Document>,
    initial_query: Option<String>,
) -> Result<(), RelishError> {
    let mut state = ReaderState::new(docs);
    state.list_title = list_title.to_string();
    if let Some(query) = initial_query {
        state.open_search(&query);
    }

    let mut guard = TerminalGuard::new()?;
    let terminal = guard.terminal_mut();
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let size = terminal.size()?;
                // Two rows of chrome: the content block's top and bottom border.
                state.viewport_rows = size.height.saturating_sub(3) as usize;
                terminal.draw(|frame| render(frame, &state))?;
            }
            Some(Ok(event)) = input.next() => {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    state.handle_key(key);
                }
            }
        }
        if state.should_quit {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn press(state: &mut ReaderState, code: KeyCode) {
        state.handle_key(key(code));
    }

    fn type_str(state: &mut ReaderState, text: &str) {
        for character in text.chars() {
            press(state, KeyCode::Char(character));
        }
    }

    fn docs() -> Vec<Document> {
        vec![
            Document {
                title: "Getting started".to_string(),
                lines: vec![
                    Line::raw("Getting started"),
                    Line::raw("install with relish setup"),
                ],
            },
            Document {
                title: "Chaos".to_string(),
                lines: vec![
                    Line::raw("Chaos"),
                    Line::raw("relish fault delay redis 200ms"),
                    Line::raw("faults expire after ten minutes"),
                ],
            },
        ]
    }

    // -- fuzzy ranking -----------------------------------------------------

    #[test]
    fn fuzzy_rank_puts_the_best_match_first() {
        let candidates = vec![
            "cluster-basics".to_string(),
            "chaos".to_string(),
            "getting-started".to_string(),
        ];
        let ranked = fuzzy_rank("chaos", &candidates);
        assert_eq!(ranked.first(), Some(&1));
    }

    #[test]
    fn fuzzy_rank_matches_subsequences_and_drops_misses() {
        let candidates = vec![
            "src/onion/ebpf/loader.rs".to_string(),
            "src/relish/commands.rs".to_string(),
            "src/smoker/ebpf.rs".to_string(),
        ];
        let ranked = fuzzy_rank("ebpf", &candidates);
        assert_eq!(ranked.len(), 2);
        assert!(ranked.contains(&0));
        assert!(ranked.contains(&2));
    }

    #[test]
    fn fuzzy_rank_empty_query_matches_nothing() {
        assert!(fuzzy_rank("", &["anything".to_string()]).is_empty());
    }

    // -- reducer -----------------------------------------------------------

    #[test]
    fn slash_opens_search_and_typing_ranks_hits() {
        let mut state = ReaderState::new(docs());
        press(&mut state, KeyCode::Char('/'));
        assert!(state.search.is_some());

        type_str(&mut state, "fault");
        let search = state.search.as_ref().unwrap();
        assert!(!search.results.is_empty());
        assert_eq!(search.results[0].doc, 1);
        assert!(search.results[0].preview.contains("fault"));
    }

    #[test]
    fn enter_on_a_hit_jumps_to_its_document_and_line() {
        let mut state = ReaderState::new(docs());
        press(&mut state, KeyCode::Char('/'));
        type_str(&mut state, "expire");
        press(&mut state, KeyCode::Enter);

        assert!(state.search.is_none());
        assert_eq!(state.selected, 1);
        assert_eq!(state.scroll, 2);
        assert_eq!(state.focus, Focus::Content);
    }

    #[test]
    fn escape_closes_search_without_moving() {
        let mut state = ReaderState::new(docs());
        press(&mut state, KeyCode::Char('/'));
        type_str(&mut state, "chaos");
        press(&mut state, KeyCode::Esc);
        assert!(state.search.is_none());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn open_search_pre_seeds_the_query() {
        let mut state = ReaderState::new(docs());
        state.open_search("fault");
        let search = state.search.as_ref().unwrap();
        assert_eq!(search.input, "fault");
        assert!(!search.results.is_empty());
    }

    #[test]
    fn list_navigation_selects_documents_and_resets_scroll() {
        let mut state = ReaderState::new(docs());
        state.scroll = 5;
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected, 1);
        assert_eq!(state.scroll, 0);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected, 1); // clamped at the end
        press(&mut state, KeyCode::Up);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn content_focus_scrolls_within_the_document() {
        let mut state = ReaderState::new(docs());
        state.selected = 1;
        state.viewport_rows = 1;
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.focus, Focus::Content);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.scroll, 1);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        // Clamped: 3 lines, 1 visible row => max scroll 2.
        assert_eq!(state.scroll, 2);
        press(&mut state, KeyCode::Up);
        assert_eq!(state.scroll, 1);
    }

    #[test]
    fn q_quits_and_backspace_edits_the_query_instead() {
        let mut state = ReaderState::new(docs());
        press(&mut state, KeyCode::Char('/'));
        type_str(&mut state, "xq");
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.search.as_ref().unwrap().input, "x");
        assert!(!state.should_quit);

        press(&mut state, KeyCode::Esc);
        press(&mut state, KeyCode::Char('q'));
        assert!(state.should_quit);
    }
}
