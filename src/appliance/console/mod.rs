//! The dashboard on an appliance's console (tty1): the only on-box UI.
//!
//! A machine with a monitor plugged in shows who it is and whether bun is
//! up, so whoever stands in front of a box can tell its name and address
//! without logging in (there's no login). Once bun answers, it's a small
//! read-only dashboard with six tabs: status, metrics, workloads, the
//! cluster, versions and logs (#2150). `bun appliance console` runs it on
//! the terminal systemd gives it (`reliaburger-console.service`).
//!
//! The parts:
//!
//! - [`state`]: what the screen shows and the reducer that changes it;
//! - [`view`]: drawing a state with ratatui, and the snapshot tests;
//! - [`local`]: what the machine itself says, bun or no bun;
//! - [`remote`]: what bun says, through relish's client, as the read-only
//!   console principal;
//! - this module: the event loop and the pollers that feed it.

pub mod bigtext;
pub mod local;
pub mod remote;
pub mod state;
pub mod view;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::Backend};
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use self::state::{ConsoleState, Msg, Tab};

/// How often the console repaints every cell, not only the ones that
/// changed: kernel and boot messages also land on tty1, and a cell the
/// dashboard didn't change would keep them otherwise.
const REPAINT_EVERY: Duration = Duration::from_secs(10);

/// Run the console on this process's controlling terminal until it goes
/// away. On an appliance that's tty1, which the unit hands it as standard
/// input (`StandardInput=tty`); over SSH it's the session's own terminal.
pub async fn run(paths: &crate::appliance::Paths) -> std::io::Result<()> {
    // BLOCKING-OK: appliance console: one open of the terminal device before the loop starts
    let mut guard = ConsoleTerminal::open()?;
    let result = event_loop(paths, &mut guard.terminal).await;
    drop(guard);
    result
}

/// The controlling terminal in raw mode, with the cursor hidden, put back
/// on every return path.
///
/// It draws on `/dev/tty`, not standard output: the unit keeps standard
/// output for the journal, where the launcher's install messages go.
/// There's no alternate screen, which the Linux console doesn't have.
struct ConsoleTerminal {
    terminal: Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>,
}

impl ConsoleTerminal {
    fn open() -> std::io::Result<Self> {
        let tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
        crossterm::terminal::enable_raw_mode()?;
        match Terminal::new(ratatui::backend::CrosstermBackend::new(tty)) {
            Ok(mut terminal) => {
                // The first draw paints every cell over the boot messages.
                if let Err(error) = terminal.clear() {
                    let _ = crossterm::terminal::disable_raw_mode();
                    return Err(error);
                }
                Ok(Self { terminal })
            }
            Err(error) => {
                let _ = crossterm::terminal::disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for ConsoleTerminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}

async fn event_loop<B: Backend>(
    paths: &crate::appliance::Paths,
    terminal: &mut Terminal<B>,
) -> std::io::Result<()> {
    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    let (tab_tx, tab_rx) = watch::channel(Tab::Status);
    let cancel = CancellationToken::new();
    spawn_pollers(paths, &tx, &tab_rx, &cancel);
    let sampler_tx = tx.clone();
    local::spawn_sampler(PathBuf::from(local::DATA_PATH), move |sample| {
        sampler_tx.blocking_send(Msg::Sample(sample)).is_ok()
    });

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut repaint = tokio::time::interval(REPAINT_EVERY);
    repaint.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut state = ConsoleState {
        now: unix_now(),
        ..ConsoleState::default()
    };
    let result = loop {
        // ratatui 0.30's `draw` returns the backend's own error type.
        if let Err(error) = terminal.draw(|frame| view::render(frame, &state)) {
            break Err(std::io::Error::other(error.to_string()));
        }
        tokio::select! {
            _ = tick.tick() => state.update(Msg::Tick(unix_now())),
            _ = repaint.tick() => {
                if let Err(error) = terminal.clear() {
                    break Err(std::io::Error::other(error.to_string()));
                }
            }
            Some(message) = rx.recv() => state.update(message),
            event = input.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    state.update(Msg::Key(key));
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => break Err(error),
                // The terminal hung up; systemd starts a new console.
                None => break Ok(()),
            },
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

/// Start one poller per source, each asking only while a tab that shows
/// it is on screen.
fn spawn_pollers(
    paths: &crate::appliance::Paths,
    tx: &mpsc::Sender<Msg>,
    tab: &watch::Receiver<Tab>,
    cancel: &CancellationToken,
) {
    let config_dir = paths.config_dir.clone();
    let every = |seconds| Duration::from_secs(seconds);
    let poller = |interval, wanted| Poller {
        every: interval,
        wanted,
        tab: tab.clone(),
        cancel: cancel.clone(),
        tx: tx.clone(),
    };

    // The header shows bun's health on every tab.
    let local_paths = paths.clone();
    poller(every(5), |_| true).spawn(move || {
        let paths = local_paths.clone();
        async move { Msg::Local(local::gather(&paths).await) }
    });
    let dir = config_dir.clone();
    poller(every(5), |tab| {
        matches!(tab, Tab::Status | Tab::Cluster | Tab::Versions)
    })
    .spawn(move || {
        let dir = dir.clone();
        async move { Msg::Summary(Box::new(remote::summary(dir).await)) }
    });
    let dir = config_dir.clone();
    poller(every(60), |tab| tab == Tab::Status).spawn(move || {
        let dir = dir.clone();
        async move { Msg::Wtf(remote::wtf(dir).await) }
    });
    let dir = config_dir.clone();
    poller(every(5), |tab| tab == Tab::Workloads).spawn(move || {
        let dir = dir.clone();
        async move { Msg::Workloads(remote::workloads(dir).await) }
    });
    let dir = config_dir;
    poller(every(5), |tab| tab == Tab::Cluster).spawn(move || {
        let dir = dir.clone();
        async move { Msg::Council(remote::council(dir).await) }
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
                        if !wanted(shown) {
                            continue;
                        }
                        interval.reset();
                    }
                }
                let shown = *tab.borrow();
                if !wanted(shown) {
                    continue;
                }
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
}
