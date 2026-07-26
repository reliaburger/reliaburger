//! Interactive Relish terminal UI.

pub mod app;
pub mod data;
pub mod fixtures;
mod input;
pub mod keys;
pub mod msg;
mod navigation;
pub mod palette;
mod state;
mod stream;
pub mod terminal;
pub mod theme;
pub mod views;

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::Backend};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use self::app::{DetailTab, Effect, TuiApp, View};
use self::data::{DataProvider, HttpDataProvider, spawn_refresh};
use self::msg::{DataUpdate, Msg};
use self::terminal::TerminalGuard;
use crate::relish::RelishError;
use crate::relish::client::BunClient;

/// Launch the interactive terminal UI against the local Bun agent.
pub async fn run() -> Result<(), RelishError> {
    let provider = Arc::new(HttpDataProvider {
        client: BunClient::default_local(),
    });
    let mut guard = TerminalGuard::new()?;
    let result = event_loop(provider, guard.terminal_mut()).await;
    drop(guard);
    result
}

async fn event_loop<P: DataProvider, B: Backend>(
    provider: Arc<P>,
    terminal: &mut Terminal<B>,
) -> Result<(), RelishError> {
    let (tx, mut rx) = mpsc::channel::<Msg>(256);
    let cancel = CancellationToken::new();
    let refresh = Arc::new(Notify::new());
    spawn_list_poller(
        Arc::clone(&provider),
        tx.clone(),
        Arc::clone(&refresh),
        cancel.clone(),
    );
    let (metrics_tx, metrics_rx) = watch::channel::<Option<(String, String)>>(None);
    spawn_metrics_poller(
        Arc::clone(&provider),
        tx.clone(),
        metrics_rx,
        cancel.clone(),
    );
    tokio::spawn({
        let provider = Arc::clone(&provider);
        let tx = tx.clone();
        let cancel = cancel.clone();
        async move {
            provider.follow_events(tx_for_stream(tx), cancel).await;
        }
    });

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut app = TuiApp::new();
    let mut log_cancel: Option<CancellationToken> = None;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                app.now_epoch = unix_now();
                terminal.draw(|frame| views::render(frame, &app))?;
            }
            Some(message) = rx.recv() => app.update(message),
            Some(Ok(event)) = input.next() => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.update(Msg::Key(key)),
                Event::Resize(width, height) => app.update(Msg::Resize(width, height)),
                _ => {}
            },
        }

        let metrics_target = match app.view() {
            View::AppDetail {
                app,
                namespace,
                tab: DetailTab::Metrics,
            } => Some((app.clone(), namespace.clone())),
            _ => None,
        };
        metrics_tx.send_replace(metrics_target);

        for effect in app.pending.drain(..) {
            match effect {
                Effect::RefreshAll => refresh.notify_one(),
                Effect::OpenLogStream { app, namespace } => {
                    if let Some(previous) = log_cancel.take() {
                        previous.cancel();
                    }
                    let stream_cancel = cancel.child_token();
                    log_cancel = Some(stream_cancel.clone());
                    let provider = Arc::clone(&provider);
                    let stream_tx = tx_for_stream(tx.clone());
                    tokio::spawn(async move {
                        provider
                            .follow_logs(app, namespace, 100, stream_tx, stream_cancel)
                            .await;
                    });
                }
                Effect::CloseLogStream => {
                    if let Some(previous) = log_cancel.take() {
                        previous.cancel();
                    }
                }
                Effect::FetchDeployHistory { app, namespace } => {
                    let provider = Arc::clone(&provider);
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let result = provider.deploy_history(&app, &namespace).await;
                        let _ = tx
                            .send(Msg::Data(DataUpdate::DeployHistory {
                                app,
                                namespace,
                                result,
                            }))
                            .await;
                    });
                }
                Effect::FetchAppMetrics { app, namespace } => {
                    let provider = Arc::clone(&provider);
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let result = provider.app_metrics(&app, &namespace).await;
                        let _ = tx.send(Msg::Data(DataUpdate::AppMetrics(result))).await;
                    });
                }
            }
        }
        if app.should_quit {
            cancel.cancel();
            return Ok(());
        }
    }
}

fn tx_for_stream(tx: mpsc::Sender<Msg>) -> mpsc::Sender<msg::StreamItem> {
    let (stream_tx, mut stream_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        while let Some(item) = stream_rx.recv().await {
            if tx.send(Msg::Stream(item)).await.is_err() {
                return;
            }
        }
    });
    stream_tx
}

fn spawn_list_poller<P: DataProvider>(
    provider: Arc<P>,
    tx: mpsc::Sender<Msg>,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => spawn_refresh(Arc::clone(&provider), tx.clone()),
                _ = refresh.notified() => spawn_refresh(Arc::clone(&provider), tx.clone()),
            }
        }
    });
}

fn spawn_metrics_poller<P: DataProvider>(
    provider: Arc<P>,
    tx: mpsc::Sender<Msg>,
    mut target: watch::Receiver<Option<(String, String)>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => {
                    let current = target.borrow().clone();
                    if let Some((app, namespace)) = current {
                        let result = provider.app_metrics(&app, &namespace).await;
                        if tx.send(Msg::Data(DataUpdate::AppMetrics(result))).await.is_err() {
                            return;
                        }
                    }
                }
                changed = target.changed() => if changed.is_err() { return; },
            }
        }
    });
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
