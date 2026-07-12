//! WebSocket stream reconnect loops.

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bun::events::ClusterEvent;
use crate::relish::client::BunClient;

use super::app::LogLine;
use super::msg::{StreamItem, StreamKind};

async fn retry_delay(cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => true,
    }
}

pub(super) async fn reconnect_logs(
    client: &BunClient,
    app: String,
    namespace: String,
    tail: usize,
    tx: mpsc::Sender<StreamItem>,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match client.ws_logs(&app, &namespace, tail).await {
            Ok(mut socket) => {
                if tx
                    .send(StreamItem::StreamUp {
                        what: StreamKind::Logs,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        frame = socket.next() => match frame {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(line))) => {
                                let item = StreamItem::LogLine(LogLine {
                                    instance: app.clone(),
                                    line: line.to_string(),
                                });
                                if tx.send(item).await.is_err() {
                                    return;
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(error)) => {
                                let _ = tx.send(StreamItem::StreamDown {
                                    what: StreamKind::Logs,
                                    error: error.to_string(),
                                }).await;
                                break;
                            }
                            None => {
                                let _ = tx.send(StreamItem::StreamDown {
                                    what: StreamKind::Logs,
                                    error: "connection closed".to_string(),
                                }).await;
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                if tx
                    .send(StreamItem::StreamDown {
                        what: StreamKind::Logs,
                        error: error.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        if !retry_delay(&cancel).await {
            return;
        }
    }
}

pub(super) async fn reconnect_events(
    client: &BunClient,
    tx: mpsc::Sender<StreamItem>,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match client.ws_events().await {
            Ok(mut socket) => {
                if tx
                    .send(StreamItem::StreamUp {
                        what: StreamKind::Events,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        frame = socket.next() => match frame {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(json))) => {
                                match serde_json::from_str::<ClusterEvent>(&json) {
                                    Ok(event) => {
                                        if tx.send(StreamItem::Event(event)).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(error) => {
                                        let _ = tx.send(StreamItem::StreamDown {
                                            what: StreamKind::Events,
                                            error: error.to_string(),
                                        }).await;
                                    }
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(error)) => {
                                let _ = tx.send(StreamItem::StreamDown {
                                    what: StreamKind::Events,
                                    error: error.to_string(),
                                }).await;
                                break;
                            }
                            None => {
                                let _ = tx.send(StreamItem::StreamDown {
                                    what: StreamKind::Events,
                                    error: "connection closed".to_string(),
                                }).await;
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                if tx
                    .send(StreamItem::StreamDown {
                        what: StreamKind::Events,
                        error: error.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        if !retry_delay(&cancel).await {
            return;
        }
    }
}
