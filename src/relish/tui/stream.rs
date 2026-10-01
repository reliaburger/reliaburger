//! WebSocket stream reconnect loops.

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bun::events::ClusterEvent;
use crate::ketchup::follow::LogFrame;
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
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                if tx.send(log_stream_item(&app, &text)).await.is_err() {
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

/// Turn one WebSocket text frame into what the reducer shows.
///
/// A frame that isn't a [`LogFrame`] is kept as a raw line rather than
/// dropped: a log line is better shown oddly than lost.
fn log_stream_item(app: &str, text: &str) -> StreamItem {
    match serde_json::from_str::<LogFrame>(text) {
        Ok(LogFrame::Warning(warning)) => StreamItem::LogWarning(warning),
        Ok(LogFrame::Line(line)) => StreamItem::LogLine(LogLine {
            instance: app.to_string(),
            line,
        }),
        Err(_) => StreamItem::LogLine(LogLine {
            instance: app.to_string(),
            line: text.to_string(),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warning_frame_becomes_a_log_warning() {
        let item = log_stream_item("web", r#"{"warning":"node n2 left the cluster"}"#);
        assert!(
            matches!(item, StreamItem::LogWarning(warning) if warning == "node n2 left the cluster")
        );
    }

    #[test]
    fn a_line_frame_keeps_its_node_label() {
        let item = log_stream_item("web", r#"{"line":"[n1 web-0] ready"}"#);
        assert!(matches!(item, StreamItem::LogLine(line) if line.line == "[n1 web-0] ready"));
    }

    #[test]
    fn an_unframed_text_message_is_shown_rather_than_dropped() {
        let item = log_stream_item("web", "plain text");
        assert!(matches!(item, StreamItem::LogLine(line) if line.line == "plain text"));
    }
}
