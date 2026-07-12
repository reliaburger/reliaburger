//! Key mapping shared by the reducer and help view.

use crossterm::event::{KeyCode, KeyEvent};

/// The single source of truth for global key help.
pub const KEYBINDINGS: &[(&str, &str, &str)] = &[
    ("a", "global", "apps"),
    ("n", "global", "nodes"),
    ("j", "global", "jobs"),
    ("e", "global", "events"),
    ("l", "global", "logs"),
    ("r", "global", "routes / refresh on lists"),
    ("s", "global", "search"),
    ("?", "global", "help"),
    (":", "global", "command palette"),
    ("q", "global", "back / quit"),
    ("Esc", "global", "back"),
    ("Enter", "lists", "open selection"),
    ("↑/↓", "lists", "move selection"),
    ("/", "lists", "filter"),
    ("Tab", "app detail", "next tab"),
    ("Shift-Tab", "app detail", "previous tab"),
    ("f", "logs", "toggle follow"),
    ("PgUp/PgDn", "streams", "scroll"),
];

/// Navigation intent derived from a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Apps,
    Nodes,
    Jobs,
    Events,
    Logs,
    Routes,
    Search,
    Help,
    Palette,
    Back,
    Enter,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Filter,
    Refresh,
    ToggleFollow,
    NextTab,
    PreviousTab,
}

/// Translate a key event into a reducer action.
pub fn key_to_action(key: KeyEvent) -> Option<KeyAction> {
    match key.code {
        KeyCode::Char('a') => Some(KeyAction::Apps),
        KeyCode::Char('n') => Some(KeyAction::Nodes),
        KeyCode::Char('j') => Some(KeyAction::Jobs),
        KeyCode::Char('e') => Some(KeyAction::Events),
        KeyCode::Char('l') => Some(KeyAction::Logs),
        KeyCode::Char('s') => Some(KeyAction::Search),
        KeyCode::Char('?') => Some(KeyAction::Help),
        KeyCode::Char(':') => Some(KeyAction::Palette),
        KeyCode::Char('q') | KeyCode::Esc => Some(KeyAction::Back),
        KeyCode::Enter => Some(KeyAction::Enter),
        KeyCode::Up => Some(KeyAction::Up),
        KeyCode::Down => Some(KeyAction::Down),
        KeyCode::PageUp => Some(KeyAction::PageUp),
        KeyCode::PageDown => Some(KeyAction::PageDown),
        KeyCode::Home => Some(KeyAction::Home),
        KeyCode::End => Some(KeyAction::End),
        KeyCode::Char('/') => Some(KeyAction::Filter),
        KeyCode::Char('r') => Some(KeyAction::Refresh),
        KeyCode::Char('f') => Some(KeyAction::ToggleFollow),
        KeyCode::Tab => Some(KeyAction::NextTab),
        KeyCode::BackTab => Some(KeyAction::PreviousTab),
        _ => None,
    }
}
