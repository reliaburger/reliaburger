//! The dashboard on an appliance's console (tty1): the only on-box UI.
//!
//! A machine with a monitor plugged in shows who it is and whether bun is
//! up, so whoever stands in front of a box can tell its name and address
//! without logging in (there's no login). Once bun answers, it's a small
//! read-only dashboard with six tabs: status, metrics, workloads, the
//! cluster, versions and logs (#2150), and an invaders game on `7`, which
//! kills nothing unless the cluster has opted in (#2151). `bun appliance console` runs it on
//! the terminal systemd gives it (`reliaburger-console.service`).
//!
//! The parts:
//!
//! - [`state`]: what the screen shows and the reducer that changes it;
//! - [`view`]: drawing a state with ratatui, and the snapshot tests;
//! - [`invaders`]: the game, one invader per instance on this node;
//! - [`local`]: what the machine itself says, bun or no bun;
//! - [`remote`]: what bun says, through relish's client, as the read-only
//!   console principal;
//! - this module: the event loop and the pollers that feed it.

pub mod bigtext;
pub mod invaders;
pub mod local;
pub mod remote;
pub mod state;
pub mod view;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::Terminal;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use self::state::{ConsoleState, Msg, Tab};

/// How often the console repaints every cell, not only the ones that
/// changed: kernel and boot messages also land on tty1, and a cell the
/// dashboard didn't change would keep them otherwise.
const REPAINT_EVERY: Duration = Duration::from_secs(10);

/// Where the console draws, and whether it reads keys there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// tty1 is its controlling terminal (`StandardInput=tty`): it reads
    /// keys, and no key quits, since it's the only thing on the screen.
    Kiosk,
    /// Another terminal, an SSH session say: keys work and `q` quits.
    Session,
    /// No terminal at all, as under an OS image from before the dashboard
    /// (0.3.0 to 0.3.2), whose unit gives the console no standard input
    /// while a self-upgraded bun runs this code. It draws on tty1 and reads
    /// no keys, rather than failing to start and restarting for ever.
    ScreenOnly,
}

/// The screen an old unit's console draws on.
const TTY1: &str = "/dev/tty1";

impl Mode {
    /// The mode for a process whose standard input is the terminal `stdin`
    /// names (`ttyname`), or isn't a terminal.
    pub fn for_terminal(stdin: Option<&std::path::Path>) -> Mode {
        match stdin {
            Some(name) if name == std::path::Path::new(TTY1) => Mode::Kiosk,
            Some(_) => Mode::Session,
            None => Mode::ScreenOnly,
        }
    }

    /// The device it draws on.
    fn device(self) -> &'static str {
        match self {
            Mode::Kiosk | Mode::Session => "/dev/tty",
            Mode::ScreenOnly => TTY1,
        }
    }

    fn reads_keys(self) -> bool {
        self != Mode::ScreenOnly
    }
}

/// Run the console until its terminal goes away: on tty1 under the unit,
/// on an SSH session's terminal by hand, or keyless on tty1 under an older
/// image's unit ([`Mode`]).
pub async fn run(paths: &crate::appliance::Paths) -> std::io::Result<()> {
    let stdin_name = nix::unistd::ttyname(std::io::stdin()).ok();
    let mode = Mode::for_terminal(stdin_name.as_deref());
    // BLOCKING-OK: appliance console: one open of the terminal device before the loop starts
    let (mut guard, mode) = ConsoleTerminal::open_or_screen_only(mode)?;
    // The first frame paints over the boot messages.
    guard.repaint()?;
    let result = event_loop(paths, &mut guard, mode).await;
    drop(guard);
    result
}

/// The console's terminal, in raw mode when it reads keys, put back on
/// every return path.
///
/// It draws on a terminal device, not standard output: the unit keeps
/// standard output for the journal, where the launcher's install messages
/// go. There's no alternate screen, which the Linux console doesn't have.
///
/// The viewport is fixed at the size the device itself reports, read with
/// `TIOCGWINSZ` on the file it draws on. crossterm's own size query asks
/// `/dev/tty` or standard output, and a console without a controlling
/// terminal, whose standard output is the journal, has neither, so it
/// failed before drawing a thing (#2150's first CI run).
struct ConsoleTerminal {
    terminal: Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>,
    /// The same device, for reading its size.
    device: std::fs::File,
    raw: bool,
}

/// The Linux console's size, for a device that reports none.
const FALLBACK_SIZE: (u16, u16) = (80, 25);

/// The columns and rows the terminal `device` reports, or
/// [`FALLBACK_SIZE`] when it reports zero.
pub fn device_size(device: &std::fs::File) -> std::io::Result<(u16, u16)> {
    use std::os::fd::AsRawFd as _;
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one `winsize` into `size`, which we own and
    // which outlives the call, and the descriptor is open for as long as
    // `device` is borrowed.
    let result = unsafe { libc::ioctl(device.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sized(size.ws_col, size.ws_row))
}

fn sized(columns: u16, rows: u16) -> (u16, u16) {
    if columns == 0 || rows == 0 {
        FALLBACK_SIZE
    } else {
        (columns, rows)
    }
}

fn area((columns, rows): (u16, u16)) -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(0, 0, columns, rows)
}

impl ConsoleTerminal {
    /// Open `mode`'s terminal, or, when that fails, tty1 without keys,
    /// which needs nothing from the unit.
    fn open_or_screen_only(mode: Mode) -> std::io::Result<(Self, Mode)> {
        match Self::open(mode) {
            Ok(terminal) => Ok((terminal, mode)),
            Err(error) if mode != Mode::ScreenOnly => {
                eprintln!(
                    "console: can't draw on {}: {error}; drawing on {TTY1} without keys",
                    mode.device()
                );
                Ok((Self::open(Mode::ScreenOnly)?, Mode::ScreenOnly))
            }
            Err(error) => Err(error),
        }
    }

    fn open(mode: Mode) -> std::io::Result<Self> {
        let device = std::fs::OpenOptions::new()
            .write(true)
            .open(mode.device())?;
        let size = device_size(&device)?;
        // Keys are a bonus: a terminal that won't go raw still gets drawn on.
        let raw = mode.reads_keys()
            && match crossterm::terminal::enable_raw_mode() {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("console: no keys, raw mode failed: {error}");
                    false
                }
            };
        let restore = || {
            if raw {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        };
        let terminal = match fresh_terminal(&device, size) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore();
                return Err(error);
            }
        };
        eprintln!(
            "console: drawing on {} at {}x{}{}",
            mode.device(),
            size.0,
            size.1,
            if raw { ", reading keys" } else { "" }
        );
        Ok(Self {
            terminal,
            device,
            raw,
        })
    }

    /// Repaint every cell from the next frame on: clear the screen, and
    /// start a new `Terminal`, whose blank last frame then matches it. Also
    /// picks up a new screen size.
    ///
    /// Not `Terminal::clear`: it asks the terminal where the cursor is and
    /// waits for the answer on standard input, which a VT under systemd
    /// never gave, so the console exited before drawing anything (#2150's
    /// first CI run).
    fn repaint(&mut self) -> std::io::Result<()> {
        use std::io::Write as _;
        let size = device_size(&self.device)?;
        self.device.write_all(CLEAR_SCREEN)?;
        self.device.flush()?;
        self.terminal = fresh_terminal(&self.device, size)?;
        Ok(())
    }
}

/// Home the cursor and clear the screen.
const CLEAR_SCREEN: &[u8] = b"\x1b[H\x1b[2J";

/// A ratatui terminal on `device` with a fixed viewport of `size`. Making
/// one asks the terminal nothing: no size query, no cursor query.
fn fresh_terminal(
    device: &std::fs::File,
    size: (u16, u16),
) -> std::io::Result<Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>> {
    let backend = ratatui::backend::CrosstermBackend::new(device.try_clone()?);
    let options = ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fixed(area(size)),
    };
    Terminal::with_options(backend, options)
}

impl Drop for ConsoleTerminal {
    fn drop(&mut self) {
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        let _ = self.terminal.show_cursor();
    }
}

/// The next terminal event, or never, for a console that reads no keys.
async fn next_event(input: &mut Option<EventStream>) -> Option<std::io::Result<Event>> {
    match input {
        Some(input) => input.next().await,
        None => std::future::pending().await,
    }
}

async fn event_loop(
    paths: &crate::appliance::Paths,
    screen: &mut ConsoleTerminal,
    mode: Mode,
) -> std::io::Result<()> {
    let keys = screen.raw;
    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    let (tab_tx, tab_rx) = watch::channel(Tab::Status);
    let cancel = CancellationToken::new();
    let remote = Arc::new(remote::Remote::new(paths.config_dir.clone()));
    spawn_pollers(paths, &remote, &tx, &tab_rx, &cancel);
    let sampler_tx = tx.clone();
    local::spawn_sampler(PathBuf::from(local::DATA_PATH), move |sample| {
        sampler_tx.blocking_send(Msg::Sample(sample)).is_ok()
    });

    let mut input = keys.then(EventStream::new);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut repaint = tokio::time::interval(REPAINT_EVERY);
    repaint.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut state = ConsoleState {
        now: unix_now(),
        can_quit: mode == Mode::Session,
        keyless: !keys,
        ..ConsoleState::default()
    };
    let result = loop {
        // ratatui 0.30's `draw` returns the backend's own error type.
        if let Err(error) = screen.terminal.draw(|frame| view::render(frame, &state)) {
            break Err(std::io::Error::other(error.to_string()));
        }
        tokio::select! {
            _ = tick.tick() => state.update(Msg::Tick(unix_now())),
            _ = repaint.tick() => {
                if let Err(error) = screen.repaint() {
                    break Err(error);
                }
            }
            Some(message) = rx.recv() => state.update(message),
            event = next_event(&mut input) => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    state.update(Msg::Key(key));
                }
                Some(Ok(_)) => {}
                // Keys are a bonus: lose them, keep the screen.
                Some(Err(error)) => {
                    eprintln!("console: stopped reading keys: {error}");
                    input = None;
                    state.keyless = true;
                }
                // The terminal hung up; systemd starts a new console.
                None => break Ok(()),
            },
        }
        if state.quit {
            break Ok(());
        }
        if let Some(invader) = state.take_kill() {
            send_kill(&remote, &tx, invader, state.local.node.clone());
        }
        tab_tx.send_if_modified(|tab| {
            let changed = *tab != state.tab;
            *tab = state.tab;
            changed
        });
    };
    cancel.cancel();
    result
}

/// Send a kill the invaders game queued in kill mode, and feed bun's
/// answer back to the game. Bun decides whether it happens.
fn send_kill(
    remote: &Arc<remote::Remote>,
    tx: &mpsc::Sender<Msg>,
    invader: invaders::Invader,
    node: Option<String>,
) {
    let remote = Arc::clone(remote);
    let tx = tx.clone();
    tokio::spawn(async move {
        let answer = match node {
            Some(node) => remote.kill(&invader, &node).await,
            None => Err("this machine isn't in a cluster yet".to_string()),
        };
        let _ = tx
            .send(Msg::KillAnswered {
                instance: invader.instance,
                answer,
            })
            .await;
    });
}

/// Start one poller per source, each asking only while a tab that shows
/// it is on screen.
fn spawn_pollers(
    paths: &crate::appliance::Paths,
    remote: &Arc<remote::Remote>,
    tx: &mpsc::Sender<Msg>,
    tab: &watch::Receiver<Tab>,
    cancel: &CancellationToken,
) {
    let every = |seconds| Duration::from_secs(seconds);
    let poller = |interval, wanted| Poller {
        every: interval,
        wanted,
        tab: tab.clone(),
        cancel: cancel.clone(),
        tx: tx.clone(),
    };
    let local = Arc::new(local::Gatherer::new(paths.clone()));
    let remote = Arc::clone(remote);

    // The header shows bun's health on every tab.
    poller(every(5), |_| true).spawn(move || {
        let local = Arc::clone(&local);
        async move { Msg::Local(local.gather().await) }
    });
    // The summary fans out to every node, so it's asked less often.
    let source = Arc::clone(&remote);
    poller(every(15), |tab| {
        matches!(tab, Tab::Status | Tab::Cluster | Tab::Versions)
    })
    .spawn(move || {
        let remote = Arc::clone(&source);
        async move { Msg::Summary(Box::new(remote.summary().await)) }
    });
    // `wtf` asks every node a dozen questions: every five minutes is plenty
    // for a screen.
    let source = Arc::clone(&remote);
    poller(every(300), |tab| tab == Tab::Status).spawn(move || {
        let remote = Arc::clone(&source);
        async move { Msg::Wtf(remote.wtf().await) }
    });
    let source = Arc::clone(&remote);
    poller(every(5), |tab| {
        matches!(tab, Tab::Workloads | Tab::Invaders)
    })
    .spawn(move || {
        let remote = Arc::clone(&source);
        async move { Msg::Workloads(remote.workloads().await) }
    });
    let source = Arc::clone(&remote);
    poller(every(15), |tab| tab == Tab::Invaders).spawn(move || {
        let remote = Arc::clone(&source);
        async move { Msg::Settings(remote.settings().await) }
    });
    poller(every(5), |tab| tab == Tab::Cluster).spawn(move || {
        let remote = Arc::clone(&remote);
        async move { Msg::Council(remote.council().await) }
    });
    poller(every(60), |tab| tab == Tab::Versions)
        .spawn(|| async { Msg::Versions(local::versions().await) });
    poller(every(5), |tab| tab == Tab::Logs)
        .spawn(|| async { Msg::Logs(local::journal(500).await) });
}

/// One source the console polls.
struct Poller {
    every: Duration,
    /// Whether the source is worth asking while this tab is on screen.
    wanted: fn(Tab) -> bool,
    tab: watch::Receiver<Tab>,
    cancel: CancellationToken,
    tx: mpsc::Sender<Msg>,
}

impl Poller {
    /// Ask `fetch` every `every` while a tab that wants it shows, and at
    /// once when one comes on screen, until the console stops. Each fetch
    /// has its own deadline, so one never waits longer than that.
    fn spawn<F, Fut>(self, fetch: F)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send,
    {
        let Poller {
            every,
            wanted,
            mut tab,
            cancel,
            tx,
        } = self;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut last: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = interval.tick() => {}
                    changed = tab.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        // Copy the tab out; the borrow ends with the statement.
                        let shown = *tab.borrow_and_update();
                        // Coming back to a tab doesn't ask again before its
                        // interval is up: flicking through the tabs mustn't
                        // run `wtf` each time.
                        let fresh = last.is_some_and(|at| at.elapsed() < every);
                        if !wanted(shown) || fresh {
                            continue;
                        }
                        interval.reset();
                    }
                }
                let shown = *tab.borrow();
                if !wanted(shown) {
                    continue;
                }
                last = Some(tokio::time::Instant::now());
                let message = tokio::select! {
                    _ = cancel.cancelled() => return,
                    message = fetch() => message,
                };
                if tx.send(message).await.is_err() {
                    return;
                }
            }
        });
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source asks only while a tab that shows it is on screen, and at
    /// once when one comes on: the Logs tab's `journalctl` doesn't run
    /// while someone reads the Status tab.
    #[tokio::test]
    async fn a_poller_asks_only_while_its_tab_shows() {
        let (tx, mut rx) = mpsc::channel(16);
        let (tab_tx, tab_rx) = watch::channel(Tab::Status);
        let cancel = CancellationToken::new();
        Poller {
            every: Duration::from_millis(20),
            wanted: |tab| tab == Tab::Logs,
            tab: tab_rx,
            cancel: cancel.clone(),
            tx,
        }
        .spawn(|| async { Msg::Logs(Ok(vec!["asked".into()])) });

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(rx.try_recv().is_err(), "asked while the Status tab showed");

        tab_tx.send_replace(Tab::Logs);
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("asked once the Logs tab showed");
        assert!(matches!(message, Some(Msg::Logs(Ok(_)))));

        cancel.cancel();
    }

    /// The unit's tty1 is a kiosk; any other terminal is a session `q`
    /// leaves; no terminal (an older image's unit, #2150's review) draws on
    /// tty1 without keys instead of failing to start.
    #[test]
    fn the_terminal_on_standard_input_picks_the_mode() {
        use std::path::Path;
        assert_eq!(
            Mode::for_terminal(Some(Path::new("/dev/tty1"))),
            Mode::Kiosk
        );
        assert_eq!(
            Mode::for_terminal(Some(Path::new("/dev/pts/3"))),
            Mode::Session
        );
        assert_eq!(Mode::for_terminal(None), Mode::ScreenOnly);
        assert_eq!(Mode::ScreenOnly.device(), "/dev/tty1");
        assert!(!Mode::ScreenOnly.reads_keys());
        assert_eq!(Mode::Kiosk.device(), "/dev/tty");
        assert!(Mode::Kiosk.reads_keys() && Mode::Session.reads_keys());
    }

    /// A terminal that reports no size gets the Linux console's.
    #[test]
    fn a_terminal_with_no_size_gets_80_by_25() {
        assert_eq!(sized(0, 0), (80, 25));
        assert_eq!(sized(160, 0), (80, 25));
        assert_eq!(sized(160, 50), (160, 50));
    }

    /// The size comes from the device the console draws on, not from
    /// `/dev/tty` or standard output, which a console started by an
    /// older image's unit doesn't have. A file that isn't a terminal says
    /// so rather than inventing a size.
    #[test]
    fn the_size_is_read_from_the_device_it_draws_on() {
        let file = tempfile::tempfile().unwrap();
        assert!(device_size(&file).is_err());
    }

    /// Switching back to a tab within its interval doesn't ask again.
    #[tokio::test]
    async fn a_poller_does_not_ask_again_when_its_tab_returns_early() {
        let (tx, mut rx) = mpsc::channel(16);
        let (tab_tx, tab_rx) = watch::channel(Tab::Status);
        let cancel = CancellationToken::new();
        Poller {
            every: Duration::from_secs(300),
            wanted: |tab| tab == Tab::Status,
            tab: tab_rx,
            cancel: cancel.clone(),
            tx,
        }
        .spawn(|| async { Msg::Tick(0) });
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("asked on start");
        tab_tx.send_replace(Tab::Logs);
        tab_tx.send_replace(Tab::Status);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(rx.try_recv().is_err(), "asked again within the interval");
        cancel.cancel();
    }
}
