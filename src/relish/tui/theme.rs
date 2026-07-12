//! Relish TUI colour palette.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(244, 162, 97);
pub const GOOD: Color = Color::Rgb(82, 183, 136);
pub const WARNING: Color = Color::Rgb(255, 183, 3);
pub const BAD: Color = Color::Rgb(230, 57, 70);
pub const MUTED: Color = Color::DarkGray;

pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn selected() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}
