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

use crate::appliance::settings::ApplianceSettings;

use super::invaders::{self, Game};
use super::local::{LocalStatus, Sample, Versions};

/// The console's tabs, in the order their number keys pick them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Status,
    Metrics,
    Workloads,
    Cluster,
    Versions,
    Logs,
    /// The invaders game ([`super::invaders`]), one invader for each
    /// instance in [`ConsoleState::workloads`]. An easter egg: only its
    /// number key opens it, and the tab bar and the arrows pass it by.
    Invaders,
}

impl Tab {
    /// The tabs the tab bar shows and the arrows step through.
    pub const SHOWN: [Tab; 6] = [
        Tab::Status,
        Tab::Metrics,
        Tab::Workloads,
        Tab::Cluster,
        Tab::Versions,
        Tab::Logs,
    ];

    /// Every tab, in order.
    pub const ALL: [Tab; 7] = [
        Tab::Status,
        Tab::Metrics,
        Tab::Workloads,
        Tab::Cluster,
        Tab::Versions,
        Tab::Logs,
        Tab::Invaders,
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
            Tab::Invaders => "Invaders",
        }
    }

    /// Its number key, 1 to 7.
    pub fn number(self) -> usize {
        Tab::ALL
            .iter()
            .position(|tab| *tab == self)
            .map_or(1, |index| index + 1)
    }

    /// The shown tab `by` steps away. The game sits between the last tab
    /// and the first: right from it is Status, left is Logs.
    fn step(self, by: isize) -> Tab {
        let count = Tab::SHOWN.len() as isize;
        let from = match self {
            Tab::Invaders if by > 0 => count - 1,
            Tab::Invaders => 0,
            _ => self.number() as isize - 1,
        };
        let index = (from + by).rem_euclid(count);
        Tab::SHOWN[index as usize]
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
    /// The cluster's `[appliance]` settings, which say whether the
    /// invaders' kill mode is on.
    pub settings: Fetched<ApplianceSettings>,
    pub game: Game,
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
    Settings(Result<ApplianceSettings, String>),
    /// What bun said to a kill the invaders sent: the fault's id, or why
    /// it refused.
    KillAnswered {
        instance: String,
        answer: invaders::KillAnswer,
    },
}

impl ConsoleState {
    /// Fold one input into the state.
    pub fn update(&mut self, msg: Msg) {
        let now = self.now;
        match msg {
            Msg::Key(key) => self.key(key),
            Msg::Tick(now) => {
                self.now = now;
                if self.tab == Tab::Invaders {
                    let count = self.invaders().len();
                    self.game.tick(count);
                }
            }
            Msg::Local(local) => self.local = local,
            Msg::Summary(result) => self.summary.record(*result, now),
            Msg::Wtf(check) => self.wtf = Some(check),
            Msg::Workloads(result) => {
                if let Ok(rows) = &result {
                    self.game.workloads_seen(rows, now);
                }
                self.workloads.record(result, now)
            }
            Msg::Council(result) => self.council.record(result, now),
            Msg::Versions(versions) => self.versions = Some(versions),
            Msg::Sample(sample) => {
                if self.samples.len() == HISTORY_LENGTH {
                    self.samples.pop_front();
                }
                self.samples.push_back(sample);
            }
            Msg::Logs(result) => self.logs.record(result, now),
            Msg::Settings(result) => self.settings.record(result, now),
            Msg::KillAnswered { instance, answer } => {
                self.game.kill_answered(instance, answer, now)
            }
        }
    }

    /// Whether a hit kills: the cluster's last answer said
    /// `console_chaos = true`. Bun checks it again for every kill, so a
    /// stale `true` here only means a refusal there.
    pub fn kill_mode(&self) -> bool {
        self.settings
            .value
            .is_some_and(|settings| settings.console_chaos)
    }

    /// The invaders on the field and waiting, from the workloads list.
    pub fn invaders(&self) -> Vec<invaders::Invader> {
        self.workloads
            .value
            .as_deref()
            .map_or_else(Vec::new, |rows| invaders::invaders(rows, &self.game.popped))
    }

    /// The kill a hit queued in kill mode, for the event loop to send.
    pub fn take_kill(&mut self) -> Option<invaders::Invader> {
        self.game.take_kill()
    }

    /// The game's own keys: the arrows move the cannon instead of
    /// changing tabs, space fires, `r` brings every invader back. `true`
    /// when the key was one of them.
    fn game_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => self.game.move_cannon(-1),
            KeyCode::Right | KeyCode::Char('l') => self.game.move_cannon(1),
            KeyCode::Char(' ') => {
                let invaders = self.invaders();
                let kill_mode = self.kill_mode();
                self.game.fire(&invaders, kill_mode);
            }
            KeyCode::Char('r') => self.game.new_wave(),
            _ => return false,
        }
        true
    }

    fn key(&mut self, key: KeyEvent) {
        if self.can_quit && key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        if self.tab == Tab::Invaders && self.game_key(key) {
            return;
        }
        let tab = match key.code {
            KeyCode::Char(digit @ '1'..='7') => {
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
        assert_eq!(state.tab, Tab::Status, "the arrows pass the game by");
        press(&mut state, KeyCode::Left);
        assert_eq!(state.tab, Tab::Logs);
        press(&mut state, KeyCode::Char('7'));
        assert_eq!(state.tab, Tab::Invaders);
        press(&mut state, KeyCode::BackTab);
        assert_eq!(state.tab, Tab::Logs);
        press(&mut state, KeyCode::Char('7'));
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, Tab::Status);
        press(&mut state, KeyCode::Char('7'));
        assert_eq!(state.tab, Tab::Invaders);
        press(&mut state, KeyCode::Char('8'));
        assert_eq!(state.tab, Tab::Invaders, "there's no eighth tab");
    }

    fn workload(id: &str) -> crate::bun::top::TopRow {
        crate::bun::top::TopRow {
            node: "node-1".into(),
            instance: crate::bun::agent::InstanceStatus {
                id: id.into(),
                app_name: "web".into(),
                namespace: "default".into(),
                state: "running".into(),
                restart_count: 0,
                host_port: None,
                exit_code: None,
                pid: None,
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: None,
                image: None,
                data_on_node: false,
                reason: None,
                sidecars: Vec::new(),
            },
            cpu_percent: None,
            memory_bytes: None,
            tasks: None,
        }
    }

    /// The Invaders tab with one invader, `web-0`, over the cannon.
    fn game_with_one_invader() -> ConsoleState {
        let mut state = ConsoleState {
            tab: Tab::Invaders,
            ..ConsoleState::default()
        };
        state.update(Msg::Workloads(Ok(vec![workload("default__web-0")])));
        state.game.cannon = 3;
        state
    }

    #[test]
    fn in_the_game_the_arrows_move_the_cannon_and_tab_still_leaves() {
        let mut state = game_with_one_invader();
        press(&mut state, KeyCode::Right);
        assert_eq!((state.tab, state.game.cannon), (Tab::Invaders, 5));
        press(&mut state, KeyCode::Left);
        assert_eq!((state.tab, state.game.cannon), (Tab::Invaders, 3));
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, Tab::Status);
    }

    #[test]
    fn a_hit_kills_nothing_until_the_cluster_turns_kill_mode_on() {
        let mut state = game_with_one_invader();
        assert!(!state.kill_mode());
        press(&mut state, KeyCode::Char(' '));
        assert!(state.game.popped.contains("default__web-0"));
        assert_eq!(state.take_kill(), None);
        assert!(state.invaders().is_empty(), "the popped invader is gone");

        press(&mut state, KeyCode::Char('r'));
        state.update(Msg::Settings(Ok(ApplianceSettings {
            console_chaos: true,
        })));
        assert!(state.kill_mode());
        press(&mut state, KeyCode::Char(' '));
        let kill = state.take_kill().expect("kill mode queues a kill");
        assert_eq!(kill.instance, "default__web-0");
        state.update(Msg::KillAnswered {
            instance: "default__web-0".into(),
            answer: invaders::KillAnswer::Refused {
                reason: "kill mode is off".into(),
            },
        });
        assert_eq!(state.invaders().len(), 1, "a refused kill brings it back");
    }

    #[test]
    fn a_failed_settings_read_keeps_kill_mode_as_last_known_and_only_true_is_on() {
        let mut state = ConsoleState::default();
        state.update(Msg::Settings(Err("bun isn't answering".into())));
        assert!(!state.kill_mode(), "unknown is off");
        state.update(Msg::Settings(Ok(ApplianceSettings::default())));
        assert!(!state.kill_mode());
    }

    #[test]
    fn the_formation_marches_only_while_the_game_shows() {
        let mut state = game_with_one_invader();
        state.update(Msg::Tick(1));
        assert_eq!(state.game.march, 1);
        press(&mut state, KeyCode::Char('1'));
        state.update(Msg::Tick(2));
        assert_eq!(state.game.march, 1);
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
