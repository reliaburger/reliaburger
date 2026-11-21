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
    ("/", "lists", "filter; Esc clears it"),
    ("/", "logs, metrics", "search; Esc clears it"),
    ("→ or Tab", "detail views", "next tab"),
    ("← or Shift-Tab", "detail views", "previous tab"),
    ("↑/↓", "instances", "select an instance"),
    (
        "k or Ctrl-K",
        "instances",
        "kill the selected instance (asks first)",
    ),
    ("↑/↓", "workloads", "select an instance"),
    ("Enter", "workloads", "open the instance's app"),
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
    Kill,
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
        // Ctrl-K too: `KeyCode` ignores the modifiers, so it lands here.
        KeyCode::Char('k') => Some(KeyAction::Kill),
        KeyCode::Tab | KeyCode::Right => Some(KeyAction::NextTab),
        KeyCode::BackTab | KeyCode::Left => Some(KeyAction::PreviousTab),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    #[test]
    fn k_and_ctrl_k_both_kill() {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            assert_eq!(
                key_to_action(KeyEvent::new(KeyCode::Char('k'), modifiers)),
                Some(KeyAction::Kill)
            );
        }
    }

    #[test]
    fn the_help_lists_the_kill_key() {
        assert!(
            KEYBINDINGS
                .iter()
                .any(|(key, context, _)| key.contains("Ctrl-K") && *context == "instances")
        );
    }
}
