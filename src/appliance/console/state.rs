//! The console's state and the one function that changes it.
//!
//! Like `relish tui` (book chapter 13), the console is a reducer: every
//! key press and every answer from a poller arrives as a [`Msg`], and
//! [`ConsoleState::update`] folds it into the state. Rendering only reads
//! the state, so a test builds a state by hand and snapshots the screen.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent};

use crate::bun::summary::ClusterSummary;
use crate::bun::summary::verdict::WtfCheck;
use crate::bun::top::TopRow;
use crate::relish::council_view::CouncilNodeObservation;

use super::local::{LocalStatus, Sample, Versions};

/// The console's tabs, in the order their number keys pick them.
///
/// The invaders game (#2150's easter egg, and #2151's kill mode) would be
/// a seventh variant, reading [`ConsoleState::workloads`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Status,
    Metrics,
    Workloads,
    Cluster,
    Versions,
    Logs,
}

impl Tab {
    /// Every tab, in order.
    pub const ALL: [Tab; 6] = [
        Tab::Status,
        Tab::Metrics,
        Tab::Workloads,
        Tab::Cluster,
        Tab::Versions,
        Tab::Logs,
    ];

    /// The tab's name, as its title and the tab bar say it.
    pub fn title(self) -> &'static str {
        match self {
            Tab::Status => "Status",
            Tab::Metrics => "Metrics",
            Tab::Workloads => "Workloads",
            Tab::Cluster => "Cluster",
            Tab::Versions => "Versions",
            Tab::Logs => "Logs",
        }
    }

    /// Its number key, 1 to 6.
    pub fn number(self) -> usize {
        Tab::ALL
            .iter()
            .position(|tab| *tab == self)
            .map_or(1, |index| index + 1)
    }

    fn step(self, by: isize) -> Tab {
        let count = Tab::ALL.len() as isize;
        let index = (self.number() as isize - 1 + by).rem_euclid(count);
        Tab::ALL[index as usize]
    }
}

/// An answer from bun that can fail, with the last good value kept: a
/// panel shows what it last knew and says how old it is.
#[derive(Debug, Clone)]
pub struct Fetched<T> {
    /// The last good answer.
    pub value: Option<T>,
    /// When it came, Unix seconds.
    pub at: Option<u64>,
    /// Why the newest try failed; `None` once one succeeds.
    pub error: Option<String>,
}

impl<T> Default for Fetched<T> {
    fn default() -> Self {
        Self {
            value: None,
            at: None,
            error: None,
        }
    }
}

impl<T> Fetched<T> {
    fn record(&mut self, result: Result<T, String>, now: u64) {
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.at = Some(now);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
}

/// How many samples the Metrics tab keeps: four minutes at one every 2 s.
pub const HISTORY_LENGTH: usize = 120;

/// Everything the console shows.
#[derive(Debug, Clone, Default)]
pub struct ConsoleState {
    pub tab: Tab,
    pub local: LocalStatus,
    pub summary: Fetched<ClusterSummary>,
    /// `wtf`'s checks; `None` before they first finish.
    pub wtf: Option<WtfCheck>,
    pub workloads: Fetched<Vec<TopRow>>,
    pub council: Fetched<Vec<CouncilNodeObservation>>,
    pub versions: Option<Versions>,
    /// The newest samples, oldest first.
    pub samples: VecDeque<Sample>,
    pub logs: Fetched<Vec<String>>,
    /// Lines scrolled back from the newest log line, or down from the top
    /// of a list.
    pub scroll: usize,
    /// Unix seconds, for "as of" and ages.
    pub now: u64,
    /// Whether `q` leaves: on a terminal other than tty1 (an SSH session),
    /// never on the machine's own screen.
    pub can_quit: bool,
    /// Set by `q` when it may leave; the event loop then stops.
    pub quit: bool,
    /// The console reads no keys: an older OS image's unit gave it no
    /// terminal, so it only draws on tty1.
    pub keyless: bool,
}

/// One input to the console.
#[derive(Debug, Clone)]
pub enum Msg {
    Key(KeyEvent),
    /// A second passed: only the clock moves.
    Tick(u64),
    Local(LocalStatus),
    /// Boxed: a summary is far bigger than any other message.
    Summary(Box<Result<ClusterSummary, String>>),
    Wtf(WtfCheck),
    Workloads(Result<Vec<TopRow>, String>),
    Council(Result<Vec<CouncilNodeObservation>, String>),
    Versions(Versions),
    Sample(Sample),
    Logs(Result<Vec<String>, String>),
}

impl ConsoleState {
    /// Fold one input into the state.
    pub fn update(&mut self, msg: Msg) {
        let now = self.now;
        match msg {
            Msg::Key(key) => self.key(key),
            Msg::Tick(now) => self.now = now,
            Msg::Local(local) => self.local = local,
            Msg::Summary(result) => self.summary.record(*result, now),
            Msg::Wtf(check) => self.wtf = Some(check),
            Msg::Workloads(result) => self.workloads.record(result, now),
            Msg::Council(result) => self.council.record(result, now),
            Msg::Versions(versions) => self.versions = Some(versions),
            Msg::Sample(sample) => {
                if self.samples.len() == HISTORY_LENGTH {
                    self.samples.pop_front();
                }
                self.samples.push_back(sample);
            }
            Msg::Logs(result) => self.logs.record(result, now),
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if self.can_quit && key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        let tab = match key.code {
            KeyCode::Char(digit @ '1'..='6') => {
                let index = digit as usize - '1' as usize;
                Some(Tab::ALL[index])
            }
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => Some(self.tab.step(1)),
            KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => Some(self.tab.step(-1)),
            _ => None,
        };
        if let Some(tab) = tab {
            if tab != self.tab {
                self.tab = tab;
                self.scroll = 0;
            }
            return;
        }
        // In the Logs tab, scrolling goes back in time; in a list, down it.
        let (back, forward) = match self.tab {
            Tab::Logs => (1, -1),
            _ => (-1, 1),
        };
        let by: isize = match key.code {
            KeyCode::Up | KeyCode::Char('k') => back,
            KeyCode::Down | KeyCode::Char('j') => forward,
            KeyCode::PageUp => back * 10,
            KeyCode::PageDown => forward * 10,
            KeyCode::Home | KeyCode::End => {
                self.scroll = 0;
                return;
            }
            _ => return,
        };
        self.scroll = self
            .scroll
            .saturating_add_signed(by)
            .min(self.scroll_limit());
    }

    /// How far the current tab can scroll.
    fn scroll_limit(&self) -> usize {
        let rows = match self.tab {
            Tab::Logs => self.logs.value.as_ref().map_or(0, Vec::len),
            Tab::Workloads => self.workloads.value.as_ref().map_or(0, Vec::len),
            Tab::Cluster => self.council.value.as_ref().map_or(0, Vec::len),
            _ => 0,
        };
        rows.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    fn press(state: &mut ConsoleState, code: KeyCode) {
        state.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn number_keys_pick_tabs_and_arrows_wrap_around() {
        let mut state = ConsoleState::default();
        assert_eq!(state.tab, Tab::Status);
        press(&mut state, KeyCode::Char('5'));
        assert_eq!(state.tab, Tab::Versions);
        press(&mut state, KeyCode::Right);
        assert_eq!(state.tab, Tab::Logs);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, Tab::Status);
        press(&mut state, KeyCode::Left);
        assert_eq!(state.tab, Tab::Logs);
        press(&mut state, KeyCode::Char('7'));
        assert_eq!(state.tab, Tab::Logs, "there's no seventh tab");
    }

    #[test]
    fn nothing_quits_the_console() {
        let mut state = ConsoleState::default();
        for code in [KeyCode::Char('q'), KeyCode::Esc, KeyCode::Char('c')] {
            press(&mut state, code);
        }
        assert_eq!(state.tab, Tab::Status);
    }

    #[test]
    fn q_leaves_only_a_session_on_another_terminal() {
        let mut session = ConsoleState {
            can_quit: true,
            ..ConsoleState::default()
        };
        press(&mut session, KeyCode::Char('q'));
        assert!(session.quit);
        let mut kiosk = ConsoleState::default();
        press(&mut kiosk, KeyCode::Char('q'));
        assert!(!kiosk.quit, "tty1's console never quits");
    }

    #[test]
    fn logs_scroll_back_and_stop_at_the_oldest_line() {
        let mut state = ConsoleState {
            tab: Tab::Logs,
            ..ConsoleState::default()
        };
        state.update(Msg::Logs(Ok(vec!["a".into(), "b".into(), "c".into()])));
        press(&mut state, KeyCode::Up);
        assert_eq!(state.scroll, 1);
        press(&mut state, KeyCode::PageUp);
        assert_eq!(state.scroll, 2, "no further back than the oldest line");
        press(&mut state, KeyCode::Down);
        assert_eq!(state.scroll, 1);
        press(&mut state, KeyCode::End);
        assert_eq!(state.scroll, 0);
        press(&mut state, KeyCode::Char('1'));
        press(&mut state, KeyCode::Char('6'));
        assert_eq!(state.scroll, 0, "a new tab starts at its newest line");
    }

    #[test]
    fn a_failed_fetch_keeps_the_last_answer_and_says_why() {
        let mut state = ConsoleState {
            now: 100,
            ..ConsoleState::default()
        };
        state.update(Msg::Logs(Ok(vec!["old".into()])));
        state.update(Msg::Tick(160));
        state.update(Msg::Logs(Err("journalctl took longer than 5 s".into())));
        assert_eq!(state.logs.value.as_deref(), Some(&["old".to_string()][..]));
        assert_eq!(state.logs.at, Some(100));
        assert_eq!(
            state.logs.error.as_deref(),
            Some("journalctl took longer than 5 s")
        );
        state.update(Msg::Logs(Ok(vec!["new".into()])));
        assert_eq!(state.logs.error, None);
        assert_eq!(state.logs.at, Some(160));
    }

    #[test]
    fn the_history_keeps_only_the_newest_samples() {
        let mut state = ConsoleState::default();
        for index in 0..HISTORY_LENGTH + 5 {
            state.update(Msg::Sample(Sample {
                cpu_percent: index as f32,
                ..Sample::default()
            }));
        }
        assert_eq!(state.samples.len(), HISTORY_LENGTH);
        assert_eq!(state.samples.front().unwrap().cpu_percent, 5.0);
    }
}
