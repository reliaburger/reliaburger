//! What a live log follow carries, whichever transport delivers it.
//!
//! A cluster-wide follow merges lines from every node that runs the app, and
//! when one of those nodes leaves or its stream breaks the follow says so and
//! carries on with the rest. SSE marks those notices with an `event: warning`
//! line; a WebSocket has no event types, so each text frame is one of these
//! values as JSON.

use serde::{Deserialize, Serialize};

/// One item of a followed log stream.
///
/// Serialised externally tagged: `{"line":"[n1 web-0] hello"}` or
/// `{"warning":"node n2 left the cluster; no longer following its logs"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFrame {
    /// A log line, prefixed `[node instance]` when it came through a
    /// cluster-wide follow.
    Line(String),
    /// A node's part of the follow stopped; the rest carries on.
    Warning(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_name_their_kind_on_the_wire() {
        let line = serde_json::to_string(&LogFrame::Line("[n1 web-0] hi".into())).unwrap();
        assert_eq!(line, r#"{"line":"[n1 web-0] hi"}"#);
        let warning = serde_json::to_string(&LogFrame::Warning("node n2 left".into())).unwrap();
        assert_eq!(warning, r#"{"warning":"node n2 left"}"#);
        let back: LogFrame = serde_json::from_str(&warning).unwrap();
        assert_eq!(back, LogFrame::Warning("node n2 left".into()));
    }
}
